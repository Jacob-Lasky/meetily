"""Localhost WebSocket server between Meetily (Rust) and the Pipecat pipelines.

Endpoints, all on 127.0.0.1 and all requiring `Authorization: Token <run
token>` (a fresh random token per recording, handed over on stdin):

- `/v2/listen?side=me|them|unknown&model=...&keyterm=...` - one per audio
  channel. Rust streams 16 kHz mono linear16 as binary messages; the sidecar
  runs a Pipecat pipeline (DeepgramFluxSTTService -> TurnRelay) and sends back
  each committed turn as the raw Flux `TurnInfo` JSON (`TranscriptionFrame.result`).
  So Rust's existing Flux lane code reads it unchanged: time mapping, trailing
  silence, bleed matching, error handling. `{"type":"CloseStream"}` ends the
  pipeline; a `{"type":"Error"}` message means the channel failed.
- `/control` - JSON both ways for live assist (see `ControlSession`).
"""

from __future__ import annotations

import asyncio
import hmac
import json
import os
from typing import Any
from urllib.parse import parse_qs, urlsplit

from loguru import logger
from pipecat.frames.frames import InterimTranscriptionFrame, TranscriptionFrame
from pipecat.processors.frame_processor import FrameProcessor
from websockets.asyncio.server import ServerConnection, serve
from websockets.datastructures import Headers
from websockets.http11 import Response

from .assist import Assist, Limits

SAMPLE_RATE = 16_000
#: How long a channel pipeline may take to drain after CloseStream.
DRAIN_TIMEOUT_SECS = 15.0
#: How long a cancelled pipeline may take to stop.
CANCEL_TIMEOUT_SECS = 5.0


class Sidecar:
    def __init__(self, *, token: str, deepgram_api_key: str):
        self._token = token
        self._deepgram_api_key = deepgram_api_key
        self._control: ServerConnection | None = None
        self.assist = Assist(emit=self._emit_control)
        self.channels_open = 0

    # ---- auth -------------------------------------------------------------

    def _authorized(self, headers: Headers) -> bool:
        value = headers.get("Authorization", "")
        scheme, _, token = value.partition(" ")
        return scheme == "Token" and hmac.compare_digest(token.encode(), self._token.encode())

    def process_request(self, connection: ServerConnection, request) -> Response | None:
        if not self._authorized(request.headers):
            logger.warning(f"rejected an unauthenticated connection to {urlsplit(request.path).path}")
            return connection.respond(401, "unauthorized\n")
        path = urlsplit(request.path).path
        if path not in ("/v2/listen", "/control"):
            return connection.respond(404, "not found\n")
        return None

    async def handler(self, ws: ServerConnection):
        url = urlsplit(ws.request.path)
        if url.path == "/control":
            await self._serve_control(ws)
        else:
            await self._serve_channel(ws, parse_qs(url.query))

    # ---- /control ---------------------------------------------------------

    def _emit_control(self, message: dict):
        ws = self._control
        if ws is None:
            logger.debug(f"no control connection; dropping {message.get('type')}")
            return
        asyncio.get_running_loop().create_task(_send_quietly(ws, json.dumps(message)))

    async def _serve_control(self, ws: ServerConnection):
        if self._control is not None:
            await ws.close(1008, "control is already connected")
            return
        self._control = ws
        logger.info("control connected")
        try:
            async for raw in ws:
                if isinstance(raw, bytes):
                    continue
                try:
                    handle_control(self.assist, json.loads(raw))
                except Exception as e:  # noqa: BLE001 - the boundary: never let one message end the connection
                    # One bad message must not drop the connection (and with
                    # it every later card).
                    logger.warning(f"control: dropped an unusable message ({type(e).__name__})")
        finally:
            self._control = None
            # Nobody can see cards or turn assist off any more: make no calls.
            self.assist.set_enabled(False)
            await self.assist.close()
            logger.info("control disconnected")

    # ---- /v2/listen -------------------------------------------------------

    async def _serve_channel(self, ws: ServerConnection, query: dict[str, list[str]]):
        from pipecat.frames.frames import EndFrame
        from pipecat.pipeline.pipeline import Pipeline
        from pipecat.pipeline.worker import PipelineParams, PipelineWorker, ProcessorUnusablePolicy
        from pipecat.workers.runner import WorkerRunner

        side = (query.get("side") or ["unknown"])[0]
        stt = build_flux_service(self._deepgram_api_key, query, eager=side == "them")
        relay = TurnRelay(ws)
        worker = PipelineWorker(
            Pipeline([stt, relay]),
            params=PipelineParams(audio_in_sample_rate=SAMPLE_RATE),
            # DO NOT leave the default (300 s): a channel that is silent for
            # five minutes (the far side on mute) would be cancelled mid-call.
            idle_timeout_secs=None,
            enable_rtvi=False,
            processor_unusable_policy=ProcessorUnusablePolicy.END,
        )
        last_error: list[str] = []

        @worker.event_handler("on_pipeline_error")
        async def _on_error(_worker, frame):
            last_error.append(str(getattr(frame, "error", frame)))
            logger.error(f"{side} channel: {last_error[-1]}")

        if side == "them":
            assist = self.assist

            @stt.event_handler("on_eager_end_of_turn")
            async def _eager(_stt, transcript):
                assist.on_eager_end_of_turn(transcript)

            @stt.event_handler("on_turn_resumed")
            async def _resumed(_stt):
                assist.on_turn_resumed()

            @stt.event_handler("on_end_of_turn")
            async def _eot(_stt, transcript):
                assist.on_end_of_turn(transcript)

        runner = WorkerRunner(handle_sigint=False)
        await runner.add_workers(worker)
        run_task = asyncio.create_task(runner.run())
        self.channels_open += 1
        logger.info(f"{side} channel open")
        try:
            pipeline_ended = await self._pump(ws, worker, run_task)
            if pipeline_ended:
                # The pipeline stopped on its own (a permanent Flux failure
                # under ProcessorUnusablePolicy.END). Say so at once, even if
                # no more audio comes (capture paused), so Rust falls back
                # instead of waiting on a silent channel.
                reason = last_error[-1] if last_error else "the Flux pipeline ended unexpectedly"
                await _send_quietly(ws, json.dumps({"type": "Error", "description": reason}))
            else:
                await worker.queue_frame(EndFrame())
                try:
                    await asyncio.wait_for(asyncio.shield(run_task), DRAIN_TIMEOUT_SECS)
                except asyncio.TimeoutError:
                    logger.warning(f"{side} channel: pipeline did not drain in {DRAIN_TIMEOUT_SECS:g} s")
                    # Final turns may be lost: report it rather than close as
                    # if the drain succeeded.
                    await _send_quietly(
                        ws,
                        json.dumps({"type": "Error", "description": f"the {side} channel did not finish within {DRAIN_TIMEOUT_SECS:g} s; its last turn may be missing"}),
                    )
                    await worker.cancel(reason="drain timeout")
                    try:
                        await asyncio.wait_for(run_task, CANCEL_TIMEOUT_SECS)
                    except asyncio.TimeoutError:
                        run_task.cancel()
        finally:
            self.channels_open -= 1
            if not run_task.done():
                run_task.cancel()
            await ws.close()
            logger.info(f"{side} channel closed")


    @staticmethod
    async def _pump(ws: ServerConnection, worker, run_task: asyncio.Task) -> bool:
        """Feed the channel's audio to the pipeline until CloseStream or the
        socket closes (False), or until the pipeline ends by itself (True).
        Waits on both at once: a pipeline that dies while no audio arrives
        is noticed straight away."""
        from pipecat.frames.frames import InputAudioRawFrame

        while True:
            recv = asyncio.ensure_future(ws.recv())
            done, _ = await asyncio.wait({recv, run_task}, return_when=asyncio.FIRST_COMPLETED)
            if run_task in done:
                # The pipeline only ends by itself on a failure (it is ended
                # on purpose only after CloseStream), so this wins even when a
                # CloseStream arrived at the same moment.
                recv.cancel()
                return True
            try:
                msg = recv.result()
            except Exception:  # noqa: BLE001 - the peer closed or the socket failed
                return False
            if isinstance(msg, bytes):
                await worker.queue_frame(InputAudioRawFrame(audio=msg, sample_rate=SAMPLE_RATE, num_channels=1))
                continue
            try:
                if json.loads(msg).get("type") == "CloseStream":
                    return False
            except (ValueError, AttributeError):
                continue


def handle_control(assist: Assist, msg: dict[str, Any]):
    """Apply one control message from Rust. Unknown types are ignored."""
    kind = msg.get("type")
    if kind == "assist_config":
        from .llm import UnsupportedProvider, make_infer

        infer, err = None, None
        limits = Limits.from_config(msg.get("limits"))
        llm = msg.get("llm")
        if msg.get("llm_error"):
            err = str(msg["llm_error"])
        elif isinstance(llm, dict):
            try:
                infer = make_infer(llm, limits.max_tokens)
            except UnsupportedProvider as e:
                err = str(e)
            except Exception as e:  # noqa: BLE001 - surfaced to the user as the assist error
                err = f"could not set up the {llm.get('provider')} LLM service: {e}"
        else:
            err = "no summary model is configured (Settings > Summary)"
        assist.configure(
            system_prompt=str(msg.get("system_prompt") or ""),
            infer=infer,
            llm_error=err,
            enabled=bool(msg.get("enabled")),
            limits=limits,
        )
        _configure_research(assist, msg.get("research"), llm if infer is not None else None)
    elif kind == "assist_enabled":
        assist.set_enabled(bool(msg.get("enabled")))
    elif kind == "line":
        assist.add_line(int(msg["seq"]), str(msg.get("side", "unknown")), str(msg.get("text", "")))
    elif kind == "retract":
        assist.retract(int(msg["seq"]))
    elif kind == "shown":
        assist.set_shown(list(msg.get("texts") or []))


def _configure_research(assist: Assist, raw: Any, llm: dict | None):
    """Set up (or turn off) the research lane from the config's `research`
    part. The MCP connections are kept when only the prompts changed."""
    from .llm import UnsupportedProvider, make_tool_loop
    from .research import MCPTools, ResearchLimits

    raw = raw if isinstance(raw, dict) else {}
    limits = ResearchLimits.from_config(raw)
    servers = [s for s in (raw.get("servers") or []) if isinstance(s, dict)]
    tools = loop = None
    key = None
    if raw.get("enabled") and servers and llm is not None:
        try:
            loop = make_tool_loop(llm, limits.max_tokens, limits.max_tool_calls)
        except UnsupportedProvider as e:
            assist._emit({"type": "research_error", "research_id": None, "question": None, "error": str(e)})
        else:
            key = json.dumps(servers, sort_keys=True)

            def on_error(error: str):
                assist._emit({"type": "research_error", "research_id": None, "question": None, "error": error})

            tools = MCPTools(servers, on_error)
    research = assist.research
    try:
        running = asyncio.get_running_loop()
    except RuntimeError:
        running = None
    if running is None:
        # Only a direct call outside the server (tests): nothing to connect.
        research.set_prompt(str(raw.get("system_prompt") or ""), limits)
        return
    # Always queued, never short-cut: configs apply in arrival order, so an
    # "off" right after an "on" still tears down what the "on" opens.
    task = running.create_task(
        research.configure(
            tools=tools,
            loop=loop,
            system_prompt=str(raw.get("system_prompt") or ""),
            limits=limits,
            config_key=key,
            epoch=research.epoch,
        )
    )
    _BACKGROUND.add(task)
    task.add_done_callback(_BACKGROUND.discard)


#: Tasks started from sync control handling; held so they are not collected.
_BACKGROUND: set[asyncio.Task] = set()


def build_flux_service(api_key: str, query: dict[str, list[str]], *, eager: bool):
    """DeepgramFluxSTTService from the query Rust's `build_flux_url` builds."""
    if os.environ.get("MEETILY_SIDECAR_FAKE_FLUX") == "1":
        from .fake import FakeFlux

        fail = os.environ.get("MEETILY_SIDECAR_FAKE_FAIL_AFTER")
        return FakeFlux(fail_after=int(fail) if fail else None)
    from pipecat.services.deepgram.flux.stt import DeepgramFluxSTTService
    from pipecat.transcriptions.language import Language

    def one(name: str) -> str | None:
        values = query.get(name)
        return values[0] if values else None

    hints = []
    for code in query.get("language_hint", []):
        try:
            hints.append(Language(code))
        except ValueError:
            logger.warning(f"ignoring unknown Flux language hint {code!r}")
    settings = DeepgramFluxSTTService.Settings(
        model=one("model") or "flux-general-en",
        keyterm=query.get("keyterm", []),
        language_hints=hints or None,
    )
    return DeepgramFluxSTTService(
        api_key=api_key,
        sample_rate=SAMPLE_RATE,
        mip_opt_out=one("mip_opt_out") == "true",
        # Eager end of turn only feeds live assist, which listens to the far
        # side; it costs nothing on the Deepgram side.
        enable_eager_end_of_turn=eager,
        settings=settings,
    )


class TurnRelay(FrameProcessor):
    """Sends each committed Flux turn back to Rust as its raw TurnInfo JSON."""

    def __init__(self, ws: ServerConnection, **kwargs):
        super().__init__(**kwargs)
        self._ws = ws
        # The turn in progress, as of Flux's last Update (words and window).
        self._last_update: dict | None = None

    async def process_frame(self, frame, direction):
        await super().process_frame(frame, direction)
        if isinstance(frame, InterimTranscriptionFrame) and isinstance(frame.result, dict):
            self._last_update = frame.result
        elif isinstance(frame, TranscriptionFrame):
            await _send_quietly(self._ws, json.dumps(turn_info_for(frame, self._last_update)))
            self._last_update = None
        await self.push_frame(frame, direction)


def turn_info_for(frame: TranscriptionFrame, last_update: dict | None) -> dict:
    """The TurnInfo to send for a committed turn.

    A turn cut short by a dropped Flux connection (Pipecat pushes its text
    unfinalized, with no result) is still the user's words, so it is sent:
    the rest of the speech becomes a new turn on the new connection, with no
    overlap. It is timed by the turn's last Update; without that it would
    have no time at all and sort at 0:00.
    """
    if isinstance(frame.result, dict):
        return frame.result
    info = dict(last_update or {})
    info.update({"type": "TurnInfo", "event": "EndOfTurn", "transcript": frame.text})
    info.setdefault("words", [])
    return info


async def _send_quietly(ws: ServerConnection, text: str):
    try:
        await ws.send(text)
    except Exception as e:  # noqa: BLE001 - the peer is gone; its handler cleans up
        logger.debug(f"send failed: {e}")


async def serve_forever(sidecar: Sidecar, stop: asyncio.Event, on_ready) -> None:
    async with serve(
        sidecar.handler,
        "127.0.0.1",
        0,
        process_request=sidecar.process_request,
        max_size=2**22,
        ping_interval=None,
    ) as server:
        port = next(iter(server.sockets)).getsockname()[1]
        on_ready(port)
        await stop.wait()
