"""Turn -> assist trigger, cancellation on a resumed turn, delivery rules."""

import asyncio

import pytest

from meetily_sidecar.assist import Assist, Limits

CARD = '{"cards":[{"kind":"ask","text":"Which plan?","quote":"pricing"}]}'


class FakeLLM:
    """Each call waits on its own gate; tests release calls by index."""

    def __init__(self, output=CARD):
        self.output = output
        self.calls: list[tuple[str, str]] = []
        self.gates: list[asyncio.Event] = []
        self.cancelled: list[int] = []
        self.error: Exception | None = None

    async def __call__(self, system, prompt):
        i = len(self.calls)
        self.calls.append((system, prompt))
        gate = asyncio.Event()
        self.gates.append(gate)
        try:
            await gate.wait()
        except asyncio.CancelledError:
            self.cancelled.append(i)
            raise
        if self.error:
            raise self.error
        return self.output

    def release(self, i):
        self.gates[i].set()


async def settle():
    for _ in range(5):
        await asyncio.sleep(0)


def make(llm=None, enabled=True, **kw):
    events: list[dict] = []
    llm = llm or FakeLLM()
    a = Assist(emit=events.append, **kw)
    a.configure(system_prompt="SYS", infer=llm, llm_error=None, enabled=enabled)
    return a, llm, events


def of(events, kind):
    return [e for e in events if e["type"] == kind]


async def test_end_of_turn_triggers_one_call_and_delivers_cards():
    a, llm, events = make()
    a.on_end_of_turn("What does pricing look like?")
    await settle()
    assert len(llm.calls) == 1
    assert llm.calls[0][0] == "SYS"
    assert llm.calls[0][1].endswith("Them: What does pricing look like?\n")
    llm.release(0)
    await settle()
    cards = of(events, "assist_cards")
    assert len(cards) == 1
    assert cards[0]["cards"] == [{"kind": "ask", "text": "Which plan?", "quote": "pricing"}]
    assert cards[0]["trigger"] == "final"
    assert cards[0]["since_end_of_turn_ms"] is not None


async def test_eager_result_is_held_until_the_turn_commits():
    a, llm, events = make()
    a.on_eager_end_of_turn("Can you send pricing")
    await settle()
    llm.release(0)
    await settle()
    assert of(events, "assist_cards") == [], "a prediction's cards must not show before EndOfTurn"
    a.on_end_of_turn("Can you send pricing?")  # same words, punctuation differs
    await settle()
    assert len(llm.calls) == 1, "the held speculative result is used, no second call"
    cards = of(events, "assist_cards")
    assert len(cards) == 1 and cards[0]["trigger"] == "eager"


async def test_eager_call_still_running_at_end_of_turn_is_kept():
    a, llm, events = make()
    a.on_eager_end_of_turn("Can you send pricing")
    await settle()
    a.on_end_of_turn("Can you send pricing")
    await settle()
    assert len(llm.calls) == 1
    llm.release(0)
    await settle()
    assert len(of(events, "assist_cards")) == 1


async def test_turn_resumed_cancels_the_speculative_call():
    a, llm, events = make()
    a.on_eager_end_of_turn("So the thing is")
    await settle()
    a.on_turn_resumed()
    await settle()
    assert llm.cancelled == [0], "the in-flight LLM request was cancelled"
    assert of(events, "assist_cancelled")[0]["reason"] == "turn resumed"
    a.on_end_of_turn("So the thing is we need batch too")
    await settle()
    assert len(llm.calls) == 2
    assert "we need batch too" in llm.calls[1][1]
    llm.release(1)
    await settle()
    assert [c["call_id"] for c in of(events, "assist_cards")] == [2]


async def test_a_committed_transcript_that_differs_replaces_the_prediction():
    a, llm, events = make()
    a.on_eager_end_of_turn("Send it")
    await settle()
    llm.release(0)
    await settle()
    a.on_end_of_turn("Send it to legal first")
    await settle()
    assert len(llm.calls) == 2
    llm.release(1)
    await settle()
    cards = of(events, "assist_cards")
    assert [c["call_id"] for c in cards] == [2], "the stale prediction's cards are dropped"


async def test_resumed_after_a_committed_turn_does_not_cancel_it():
    a, llm, events = make()
    a.on_end_of_turn("Pricing?")
    await settle()
    a.on_turn_resumed()
    await settle()
    assert llm.cancelled == []


async def test_turns_ending_during_a_call_wait_and_only_the_newest_runs():
    a, llm, events = make()
    a.on_end_of_turn("first")
    await settle()
    a.on_end_of_turn("second")
    a.on_end_of_turn("third")
    a.on_eager_end_of_turn("fourth maybe")  # ignored while a committed call runs
    await settle()
    assert len(llm.calls) == 1
    llm.release(0)
    await settle()
    assert len(llm.calls) == 2
    assert llm.calls[1][1].endswith("Them: third\n")


async def test_disabled_makes_no_calls_and_turning_off_cancels():
    a, llm, events = make(enabled=False)
    a.on_eager_end_of_turn("x")
    a.on_end_of_turn("x")
    await settle()
    assert llm.calls == []
    a.set_enabled(True)
    a.on_end_of_turn("pricing?")
    await settle()
    a.set_enabled(False)
    await settle()
    assert llm.cancelled == [0]
    assert of(events, "assist_cards") == []


async def test_malformed_output_is_no_cards_not_an_error():
    a, llm, events = make(FakeLLM(output="Sure! I think they want pricing."))
    a.on_end_of_turn("pricing?")
    await settle()
    llm.release(0)
    await settle()
    assert of(events, "assist_cards")[0]["cards"] == []
    assert of(events, "assist_error") == []


async def test_provider_failure_is_reported():
    llm = FakeLLM()
    llm.error = RuntimeError("401 invalid x-api-key")
    a, _, events = make(llm)
    a.on_end_of_turn("pricing?")
    await settle()
    llm.release(0)
    await settle()
    assert of(events, "assist_error")[0]["error"] == "401 invalid x-api-key"


async def test_a_slow_call_times_out():
    a, llm, events = make(limits=Limits(call_timeout_secs=0.05))
    a.on_end_of_turn("pricing?")
    await asyncio.sleep(0.1)
    await settle()
    assert "timed out" in of(events, "assist_error")[0]["error"]


async def test_no_llm_reports_why():
    events = []
    a = Assist(emit=events.append)
    a.configure(system_prompt="", infer=None, llm_error="no summary model is configured", enabled=True)
    a.on_end_of_turn("pricing?")
    assert of(events, "assist_error")[0]["error"] == "no summary model is configured"


async def test_context_follows_rust_lines_retractions_and_shown_cards():
    a, llm, events = make()
    a.add_line(1, "them", "We run forty thousand calls a day.")
    a.add_line(2, "me", "We run forty thousand calls a day.")  # bleed
    a.add_line(3, "me", "Got it.")
    a.retract(2)
    a.set_shown(["Average call length?"])
    a.add_line(4, "them", "What does pricing look like?")
    a.on_end_of_turn("What does pricing look like?")
    await settle()
    prompt = llm.calls[0][1]
    assert prompt == (
        "Transcript, oldest first:\n"
        "Them: We run forty thousand calls a day.\n"
        "Me: Got it.\n"
        "Them: What does pricing look like?\n"
        "\nCards Me already has or dismissed (do not repeat or rephrase these):\n"
        "- Average call length?\n"
    ), "retracted bleed line gone, the turn's own line not duplicated"


async def test_context_window_drops_old_lines():
    now = [0.0]
    a, llm, _ = make(clock=lambda: now[0])
    a.add_line(1, "them", "ancient")
    now[0] = 121.0
    a.add_line(2, "them", "fresh")
    a.on_end_of_turn("fresh")
    await settle()
    assert "ancient" not in llm.calls[0][1]


async def test_close_cancels_the_call_in_flight():
    a, llm, events = make()
    a.on_end_of_turn("pricing?")
    await settle()
    await a.close()
    assert llm.cancelled == [0]


async def test_a_repeated_identical_turn_is_still_the_last_line_of_the_prompt():
    a, llm, _ = make()
    a.add_line(1, "them", "Yes.")
    a.add_line(2, "me", "So batch too?")
    # The second "Yes." ended before Rust's line for it arrived.
    a.on_end_of_turn("Yes.")
    await settle()
    assert llm.calls[0][1].endswith("Them: Yes.\nMe: So batch too?\nThem: Yes.\n")


async def test_limits_come_from_rust():
    a, llm, _ = make()
    a.configure(system_prompt="S", infer=llm, llm_error=None, enabled=True, limits=Limits(max_lines=2))
    for i in range(4):
        a.add_line(i, "me", f"line {i}")
    a.on_end_of_turn("now")
    await settle()
    assert "line 1" not in llm.calls[0][1] and "line 3" in llm.calls[0][1]
    assert Limits.from_config({"max_lines": 5, "call_timeout_secs": 3}).max_lines == 5
    assert Limits.from_config(None) == Limits()
    bad = {"max_lines": -1, "call_timeout_secs": float("inf"), "max_age_secs": "x", "max_tokens": True}
    assert Limits.from_config(bad) == Limits(), "bad values keep the defaults"


async def test_malformed_output_is_not_logged(caplog_loguru):
    secret = "the wire code is 4471-9920"
    a, llm, _ = make(FakeLLM(output=f"Sure! They said {secret}."))
    a.on_end_of_turn("pricing?")
    await settle()
    llm.release(0)
    await settle()
    assert secret not in caplog_loguru.text
    assert "chars" in caplog_loguru.text


async def test_emitted_events_match_the_fixture_shared_with_rust():
    import json
    from pathlib import Path

    fixture = json.loads((Path(__file__).parent / "fixtures" / "control_events.json").read_text())
    expected = {}
    for e in fixture["events"]:
        expected.setdefault(e["type"], set(e))
    events = []
    llm = FakeLLM()
    a = Assist(emit=events.append)
    a.configure(system_prompt="", infer=llm, llm_error=None, enabled=True)
    a.on_eager_end_of_turn("so the")
    await settle()
    a.on_turn_resumed()
    a.on_end_of_turn("so the plan?")
    await settle()
    llm.release(1)
    await settle()
    llm.error = RuntimeError("boom")
    a.on_end_of_turn("again?")
    await settle()
    llm.release(2)
    await settle()
    b = Assist(emit=events.append)
    b.configure(system_prompt="", infer=None, llm_error="no model", enabled=True)
    b.on_end_of_turn("x")
    seen = {e["type"] for e in events}
    assert seen == set(expected), seen
    for e in events:
        assert set(e) == expected[e["type"]], (e["type"], set(e) ^ expected[e["type"]])
