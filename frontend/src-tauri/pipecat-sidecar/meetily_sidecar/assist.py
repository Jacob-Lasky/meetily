"""Turn-triggered live assist.

Flux's turn events on the Speaker-Out ("them") channel are the trigger, in
place of the Rust engine's 1.5 s settle timer:

- EagerEndOfTurn starts a speculative call on the predicted transcript. Its
  result is held, never shown, until the turn is committed.
- TurnResumed withdraws the prediction: the speculative call is cancelled.
- EndOfTurn commits the turn. If the committed transcript matches the
  prediction, the speculative call is kept (its result goes out as soon as it
  is ready, which is where the latency win comes from); otherwise it is
  cancelled and a call on the committed transcript starts.

One committed call runs at a time. A turn that ends while one runs waits, and
only the newest waiting turn is kept (the older one's context is in it).

The context window is the transcript lines Rust sends (`add_line`), after
bleed suppression, keyed by Rust's sequence ids, so a retracted Mic-In line
also leaves the prompt. The turn that triggered the call is appended when Rust
has not sent its line yet (it is still on its way, or only predicted).

Pure asyncio: the LLM is an injected coroutine, so tests drive this with fakes.
"""

from __future__ import annotations

import asyncio
import math
import time
from collections import deque
from dataclasses import dataclass, field
from typing import Awaitable, Callable

from loguru import logger

from .prompt import ParseError, normalize, parse_cards, user_prompt

Infer = Callable[[str, str], Awaitable[str]]
Emit = Callable[[dict], None]

@dataclass(frozen=True)
class Limits:
    """Call timeout, context window and token cap. Their one home is Rust
    (`live_assist`: CALL_TIMEOUT, engine Config, CLAUDE_MAX_TOKENS), which
    sends them in the assist config; these defaults only apply until it has.
    """

    call_timeout_secs: float = 20.0
    max_lines: int = 12
    max_age_secs: float = 120.0
    max_tokens: int = 1024

    @classmethod
    def from_config(cls, raw) -> "Limits":
        """Limits from Rust's config; a missing, non-numeric, non-finite or
        non-positive value keeps its default."""
        base = cls()
        if not isinstance(raw, dict):
            return base

        def seconds(name):
            value = raw.get(name)
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                return getattr(base, name)
            try:
                ok = math.isfinite(value) and value > 0
            except OverflowError:  # an int too big for a float
                ok = False
            return float(value) if ok else getattr(base, name)

        def count(name):
            # A whole number of at least 1: 0.5 must not become 0 lines.
            value = raw.get(name)
            if isinstance(value, bool) or not isinstance(value, int) or value < 1:
                return getattr(base, name)
            return value

        return cls(
            call_timeout_secs=seconds("call_timeout_secs"),
            max_lines=count("max_lines"),
            max_age_secs=seconds("max_age_secs"),
            max_tokens=count("max_tokens"),
        )


#: Most shown-card texts kept for a prompt (Rust sends at most 20).
MAX_SHOWN_IN_PROMPT = 20


@dataclass
class _Line:
    seq: int
    side: str
    text: str
    at: float


@dataclass
class _Call:
    call_id: int
    text: str
    trigger: str  # "eager" | "final"
    started_at: float
    confirmed: bool = False
    end_of_turn_at: float | None = None
    task: asyncio.Task | None = None
    output: str | None = None
    error: str | None = None
    done: bool = False
    meta: dict = field(default_factory=dict)


class Assist:
    def __init__(
        self,
        emit: Emit,
        infer: Infer | None = None,
        *,
        clock: Callable[[], float] = time.monotonic,
        limits: Limits = Limits(),
    ):
        self._emit = emit
        self._infer = infer
        self._clock = clock
        self.limits = limits
        self._enabled = False
        self._system_prompt = ""
        self._llm_error: str | None = "live assist is not configured yet"
        self._lines: deque[_Line] = deque()
        self._shown: list[str] = []
        self._call: _Call | None = None
        self._waiting_turn: tuple[str, float] | None = None
        self._next_call_id = 0
        self.calls_started = 0

    # ---- configuration (from Rust over /control) --------------------------

    def configure(
        self,
        *,
        system_prompt: str,
        infer: Infer | None,
        llm_error: str | None,
        enabled: bool,
        limits: Limits | None = None,
    ):
        self._system_prompt = system_prompt
        if limits is not None:
            self.limits = limits
        self._infer = infer
        self._llm_error = llm_error if infer is None else None
        self.set_enabled(enabled)

    def set_enabled(self, enabled: bool):
        if self._enabled and not enabled:
            self._waiting_turn = None
            self._cancel_current("assist turned off")
        self._enabled = enabled
        logger.info(f"live assist: {'enabled' if enabled else 'disabled'}")

    @property
    def enabled(self) -> bool:
        return self._enabled

    def add_line(self, seq: int, side: str, text: str):
        text = text.strip()
        if not text:
            return
        self._lines.append(_Line(seq, side, text, self._clock()))
        self._prune()

    def retract(self, seq: int):
        self._lines = deque(line for line in self._lines if line.seq != seq)

    def set_shown(self, texts: list[str]):
        self._shown = [t for t in texts if isinstance(t, str)][:MAX_SHOWN_IN_PROMPT]

    # ---- Flux turn events on the "them" channel ---------------------------

    def on_eager_end_of_turn(self, text: str):
        if not self._can_call() or not text.strip():
            return
        call = self._call
        if call is not None:
            if call.confirmed:
                # A committed call is running; its EndOfTurn will queue.
                return
            self._cancel_current("superseded by a newer prediction")
        self._start(text, "eager")

    def on_turn_resumed(self):
        call = self._call
        if call is not None and not call.confirmed:
            self._cancel_current("turn resumed")

    def on_end_of_turn(self, text: str):
        if not text.strip():
            return
        now = self._clock()
        if not self._can_call():
            if self._enabled and self._llm_error:
                self._emit({"type": "assist_error", "call_id": None, "error": self._llm_error})
            return
        call = self._call
        if call is not None and not call.confirmed:
            if normalize(call.text) == normalize(text):
                call.confirmed = True
                call.end_of_turn_at = now
                logger.info(f"live assist: call {call.call_id} kept (prediction held)")
                if call.done:
                    self._finish(call)
                return
            self._cancel_current("committed transcript differs from the prediction")
        elif call is not None:
            self._waiting_turn = (text, now)
            return
        self._start(text, "final", end_of_turn_at=now)

    # ---- internals --------------------------------------------------------

    def _can_call(self) -> bool:
        return self._enabled and self._infer is not None

    def _prune(self):
        now = self._clock()
        while len(self._lines) > self.limits.max_lines:
            self._lines.popleft()
        while self._lines and now - self._lines[0].at > self.limits.max_age_secs:
            self._lines.popleft()

    def _prompt_for(self, turn_text: str) -> str:
        self._prune()
        lines = [(line.side, line.text) for line in self._lines]
        # Append the triggering turn unless Rust's line for it is already the
        # newest line. Comparing with the newest "them" line instead would
        # take an earlier identical turn ("Yes.") for this one and leave it out.
        # Limit: two identical far-side turns back to back, the second ending
        # before Rust's line for it arrives, show as one "Them: Yes." line;
        # the prompt then lacks only an adjacent repeat of the same words.
        if not lines or lines[-1][0] != "them" or normalize(lines[-1][1]) != normalize(turn_text):
            lines.append(("them", turn_text))
        return user_prompt(lines, self._shown)

    def _start(self, text: str, trigger: str, end_of_turn_at: float | None = None):
        self._next_call_id += 1
        self.calls_started += 1
        call = _Call(
            call_id=self._next_call_id,
            text=text,
            trigger=trigger,
            started_at=self._clock(),
            confirmed=trigger == "final",
            end_of_turn_at=end_of_turn_at,
        )
        prompt = self._prompt_for(text)
        self._call = call
        logger.info(f"live assist: call {call.call_id} ({trigger}) started")
        self._emit({"type": "assist_started", "call_id": call.call_id, "trigger": trigger})
        call.task = asyncio.get_running_loop().create_task(self._run(call, prompt))

    async def _run(self, call: _Call, prompt: str):
        assert self._infer is not None
        try:
            call.output = await asyncio.wait_for(
                self._infer(self._system_prompt, prompt), self.limits.call_timeout_secs
            )
        except asyncio.CancelledError:
            raise
        except asyncio.TimeoutError:
            call.error = f"timed out after {self.limits.call_timeout_secs:g} s"
        except Exception as e:  # noqa: BLE001 - any provider failure is that call's error
            call.error = str(e) or type(e).__name__
        call.done = True
        call.meta["call_ms"] = round((self._clock() - call.started_at) * 1000)
        logger.info(
            f"live assist: call {call.call_id} finished in {call.meta['call_ms']} ms "
            f"(ok={call.error is None}, confirmed={call.confirmed})"
        )
        if self._call is call and call.confirmed:
            self._finish(call)

    def _finish(self, call: _Call):
        """Deliver a committed call's result, then start the waiting turn."""
        self._call = None
        since_eot = None
        if call.end_of_turn_at is not None:
            since_eot = round((self._clock() - call.end_of_turn_at) * 1000)
        if call.error is not None:
            self._emit({"type": "assist_error", "call_id": call.call_id, "error": call.error})
        else:
            try:
                cards = parse_cards(call.output or "")
            except ParseError as e:
                # Malformed output is that turn's "no cards", not an error state.
                # DO NOT log the output itself: it can echo what was said in
                # the meeting, and these lines end up in Meetily's log.
                logger.warning(f"live assist: call {call.call_id}: {e} ({len(call.output or '')} chars)")
                cards = []
            logger.info(
                f"live assist: call {call.call_id} returned {len(cards)} card(s), "
                f"{since_eot} ms after the Speaker-Out end of turn"
            )
            self._emit(
                {
                    "type": "assist_cards",
                    "call_id": call.call_id,
                    "trigger": call.trigger,
                    "cards": [c.as_json() for c in cards],
                    "call_ms": call.meta.get("call_ms"),
                    "since_end_of_turn_ms": since_eot,
                }
            )
        waiting, self._waiting_turn = self._waiting_turn, None
        if waiting is not None and self._can_call():
            self._start(waiting[0], "final", end_of_turn_at=waiting[1])

    def _cancel_current(self, reason: str):
        call, self._call = self._call, None
        if call is None:
            return
        if call.task is not None and not call.task.done():
            call.task.cancel()
        logger.info(f"live assist: call {call.call_id} cancelled ({reason})")
        self._emit({"type": "assist_cancelled", "call_id": call.call_id, "reason": reason})

    async def close(self):
        self._waiting_turn = None
        call = self._call
        self._cancel_current("session ended")
        if call is not None and call.task is not None:
            try:
                await call.task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
