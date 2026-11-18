"""The localhost server: auth, the channel pipeline lifecycle (with a fake Flux
service in the real Pipecat pipeline), the control protocol, and the
stdin/stdout handshake of the real entry point."""

import asyncio
import json
import os
import secrets
import sys
from pathlib import Path

import pytest
import websockets

from meetily_sidecar import server
from meetily_sidecar.fake import FakeFlux
from meetily_sidecar.server import Sidecar, handle_control, serve_forever, turn_info_for
from pipecat.frames.frames import TranscriptionFrame

TOKEN = "t" * 40
PROJECT = Path(__file__).resolve().parents[1]


@pytest.fixture
async def sidecar(monkeypatch):
    made = {}

    def fake_flux(api_key, query, *, eager):
        made.setdefault("queries", []).append((query, eager))
        return FakeFlux(fail_after=made.get("fail_after"))

    monkeypatch.setattr(server, "build_flux_service", fake_flux)
    sc = Sidecar(token=TOKEN, deepgram_api_key="dg")
    stop = asyncio.Event()
    ready = asyncio.get_running_loop().create_future()
    task = asyncio.create_task(serve_forever(sc, stop, ready.set_result))
    port = await ready
    yield sc, f"ws://127.0.0.1:{port}", made
    stop.set()
    await task


def auth(token=TOKEN):
    return {"Authorization": f"Token {token}"}


async def test_rejects_a_wrong_or_missing_token(sidecar):
    _, base, _ = sidecar
    for headers in ({}, auth("nope"), {"Authorization": f"Bearer {TOKEN}"}):
        with pytest.raises(websockets.exceptions.InvalidStatus) as e:
            async with websockets.connect(f"{base}/control", additional_headers=headers):
                pass
        assert e.value.response.status_code == 401


async def test_unknown_path_is_404(sidecar):
    _, base, _ = sidecar
    with pytest.raises(websockets.exceptions.InvalidStatus) as e:
        async with websockets.connect(f"{base}/other", additional_headers=auth()):
            pass
    assert e.value.response.status_code == 404


async def test_channel_relays_turninfo_and_drains_on_close_stream(sidecar):
    _, base, made = sidecar
    url = f"{base}/v2/listen?side=me&model=flux-general-en&keyterm=Deepgram&mip_opt_out=true"
    async with websockets.connect(url, additional_headers=auth()) as ws:
        for _ in range(2):
            await ws.send(b"\0" * 3200)
        await ws.send(json.dumps({"type": "CloseStream"}))
        got = [json.loads(m) async for m in ws]
    assert [m["transcript"] for m in got] == ["turn 1", "turn 2"]
    assert got[0]["words"][0]["start"] == 0.0, "word timings reach Rust"
    query, eager = made["queries"][0]
    assert query["keyterm"] == ["Deepgram"] and eager is False


async def test_a_failed_pipeline_reports_an_error_instead_of_going_silent(sidecar):
    _, base, made = sidecar
    made["fail_after"] = 0
    async with websockets.connect(f"{base}/v2/listen?side=them", additional_headers=auth()) as ws:
        msgs = []
        for _ in range(20):
            try:
                await ws.send(b"\0" * 3200)
            except websockets.ConnectionClosed:
                break
            await asyncio.sleep(0.02)
        msgs = [json.loads(m) async for m in ws]
    assert msgs and msgs[-1]["type"] == "Error"
    assert "Flux refused the key" in msgs[-1]["description"]


async def test_them_turns_reach_assist_and_cards_reach_control(sidecar, monkeypatch):
    sc, base, _ = sidecar
    calls = []

    async def infer(system, prompt):
        calls.append(prompt)
        return '{"cards":[{"kind":"todo","text":"Send pricing","quote":"turn 1"}]}'

    monkeypatch.setattr("meetily_sidecar.llm.make_infer", lambda cfg, max_tokens: infer)
    async with websockets.connect(f"{base}/control", additional_headers=auth()) as control:
        await control.send(
            json.dumps(
                {"type": "assist_config", "enabled": True, "system_prompt": "SYS", "llm": {"provider": "claude"}}
            )
        )
        await asyncio.sleep(0.05)
        async with websockets.connect(f"{base}/v2/listen?side=them", additional_headers=auth()) as ch:
            await ch.send(b"\0" * 3200)
            started = json.loads(await asyncio.wait_for(control.recv(), 5))
            cards = json.loads(await asyncio.wait_for(control.recv(), 5))
            await ch.send(json.dumps({"type": "CloseStream"}))
    assert started["type"] == "assist_started"
    assert cards["type"] == "assist_cards"
    assert cards["cards"] == [{"kind": "todo", "text": "Send pricing", "quote": "turn 1"}]
    assert calls and calls[0].endswith("Them: turn 1\n")


async def test_only_one_control_connection(sidecar):
    _, base, _ = sidecar
    async with websockets.connect(f"{base}/control", additional_headers=auth()):
        async with websockets.connect(f"{base}/control", additional_headers=auth()) as second:
            with pytest.raises(websockets.ConnectionClosed):
                await asyncio.wait_for(second.recv(), 5)


def test_control_messages_drive_assist():
    from meetily_sidecar.assist import Assist

    a = Assist(emit=lambda m: None)
    handle_control(a, {"type": "assist_config", "enabled": True, "system_prompt": "S", "llm": None})
    assert a.enabled and a._infer is None and "no summary model" in a._llm_error
    handle_control(a, {"type": "assist_config", "enabled": True, "llm": {"provider": "builtin-ai", "model": "m"}})
    assert "cannot use the builtin-ai" in a._llm_error
    handle_control(a, {"type": "assist_config", "enabled": False, "llm_error": "API key not found for claude"})
    assert not a.enabled and a._llm_error == "API key not found for claude"
    handle_control(a, {"type": "line", "seq": 4, "side": "them", "text": "hi"})
    handle_control(a, {"type": "retract", "seq": 4})
    handle_control(a, {"type": "shown", "texts": ["a", 5]})
    handle_control(a, {"type": "mystery"})
    assert list(a._lines) == [] and a._shown == ["a"]


async def test_entry_point_handshake_auth_and_exit_on_stdin_eof():
    proc = await asyncio.create_subprocess_exec(
        sys.executable,
        "-m",
        "meetily_sidecar",
        cwd=PROJECT,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        env={**os.environ, "PYTHONPATH": str(PROJECT)},
    )
    token = secrets.token_hex(32)
    proc.stdin.write(json.dumps({"token": token, "deepgram_api_key": "dg"}).encode() + b"\n")
    await proc.stdin.drain()
    line = await asyncio.wait_for(proc.stdout.readline(), 30)
    port = json.loads(line)["port"]
    with pytest.raises(websockets.exceptions.InvalidStatus):
        async with websockets.connect(f"ws://127.0.0.1:{port}/control"):
            pass
    async with websockets.connect(f"ws://127.0.0.1:{port}/control", additional_headers=auth(token)):
        pass
    proc.stdin.close()
    assert await asyncio.wait_for(proc.wait(), 15) == 0
    rest = await proc.stdout.read()
    assert rest == b"", "nothing but the handshake line may reach stdout"


async def test_entry_point_refuses_a_short_token():
    proc = await asyncio.create_subprocess_exec(
        sys.executable,
        "-m",
        "meetily_sidecar",
        cwd=PROJECT,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL,
    )
    out, _ = await asyncio.wait_for(proc.communicate(b'{"token":"short","deepgram_api_key":"k"}\n'), 30)
    assert proc.returncode == 2 and out == b""


async def test_a_pipeline_failure_is_reported_even_when_no_more_audio_comes(sidecar):
    _, base, made = sidecar
    made["fail_after"] = 0
    async with websockets.connect(f"{base}/v2/listen?side=me", additional_headers=auth()) as ws:
        await ws.send(b"\0" * 3200)  # one turn's worth, then nothing (capture paused)
        msg = json.loads(await asyncio.wait_for(ws.recv(), 10))
    assert msg["type"] == "Error" and "Flux refused the key" in msg["description"]


async def test_losing_control_turns_assist_off(sidecar, monkeypatch):
    sc, base, _ = sidecar

    async def infer(system, prompt):
        return "{}"

    monkeypatch.setattr("meetily_sidecar.llm.make_infer", lambda cfg, max_tokens: infer)
    async with websockets.connect(f"{base}/control", additional_headers=auth()) as control:
        await control.send(json.dumps({"type": "assist_config", "enabled": True, "llm": {"provider": "claude"}}))
        await asyncio.sleep(0.05)
        assert sc.assist.enabled
    await asyncio.sleep(0.1)
    assert not sc.assist.enabled, "no calls nobody can see"


async def test_a_bad_control_message_does_not_drop_the_connection(sidecar):
    sc, base, _ = sidecar
    async with websockets.connect(f"{base}/control", additional_headers=auth()) as control:
        await control.send(json.dumps({"type": "line", "side": "me", "text": "no seq"}))
        await control.send("not json")
        await control.send(json.dumps({"type": "assist_enabled", "enabled": True}))
        await asyncio.sleep(0.1)
        assert sc.assist.enabled, "later messages still apply"


def test_a_cut_short_turn_keeps_the_last_update_timing():
    update = {"type": "TurnInfo", "event": "Update", "transcript": "so the", "audio_window_start": 41.0,
              "audio_window_end": 42.5, "words": [{"word": "so", "start": 41.2, "end": 41.4}]}
    info = turn_info_for(TranscriptionFrame("so the thing", "", ""), update)
    assert info["event"] == "EndOfTurn" and info["transcript"] == "so the thing"
    assert info["words"][0]["start"] == 41.2, "timed, not 0:00"
    bare = turn_info_for(TranscriptionFrame("hi", "", ""), None)
    assert bare["words"] == [] and bare["type"] == "TurnInfo"
    committed = {"type": "TurnInfo", "event": "EndOfTurn", "transcript": "x"}
    assert turn_info_for(TranscriptionFrame("x", "", "", result=committed), update) is committed
