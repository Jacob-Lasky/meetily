"""Research lane (WORK-581): lookups beside the fast cards."""

from __future__ import annotations

import asyncio
import sys
import textwrap
from types import SimpleNamespace

import pytest

from meetily_sidecar.assist import Assist
from meetily_sidecar.llm import _claude_loop, _openai_loop, make_tool_loop
from meetily_sidecar.prompt import parse_research
from meetily_sidecar.research import MCPTools, Research, ResearchLimits, ToolSpec


class FakeTools:
    def __init__(self, specs=None, answers=None, fail_list=None):
        self.specs = specs if specs is not None else [ToolSpec("search_knowledge", "Search", {"type": "object"}, "DeepHive")]
        self.answers = answers or {}
        self.calls: list[tuple[str, dict]] = []
        self.closed = 0
        self.fail_list = fail_list

    async def list(self):
        if self.fail_list:
            raise RuntimeError(self.fail_list)
        return self.specs

    async def call(self, name, arguments):
        self.calls.append((name, arguments))
        return self.answers.get(name, "nothing")

    async def close(self):
        self.closed += 1


def loop_that(reply: str, tool_calls=(("search_knowledge", {"query": "q"}),), delay=0.0):
    async def loop(system, question, tools, call_tool):
        for name, args in tool_calls:
            await call_tool(name, args)
        if delay:
            await asyncio.sleep(delay)
        return reply

    return loop


async def settle():
    for _ in range(20):
        await asyncio.sleep(0)


async def configured(emitted, *, tools=None, loop=None, key="k"):
    r = Research(emitted.append)
    await r.configure(
        tools=tools or FakeTools(), loop=loop or loop_that('{"cards":[]}'), system_prompt="R", limits=ResearchLimits(), config_key=key
    )
    return r


def test_parse_research():
    assert parse_research('{"cards":[],"research":"  Nova-3   streaming price? "}') == "Nova-3 streaming price?"
    assert parse_research('{"cards":[]}') is None
    assert parse_research('{"cards":[],"research":5}') is None
    assert parse_research('{"cards":[],"research":""}') is None
    assert parse_research("[]") is None
    assert parse_research("no json") is None
    assert parse_research('{"research":"' + "x" * 400 + '"}') is None


def test_limits_from_config():
    assert ResearchLimits.from_config({"timeout_secs": 30, "max_tool_calls": 2, "max_tokens": 512}) == ResearchLimits(30.0, 2, 512)
    assert ResearchLimits.from_config({"timeout_secs": -1, "max_tool_calls": 0, "max_tokens": True}) == ResearchLimits()
    assert ResearchLimits.from_config(None) == ResearchLimits()


async def test_a_lookup_emits_one_card_quoting_the_turn_and_naming_its_source():
    emitted: list[dict] = []
    tools = FakeTools(answers={"search_knowledge": "Nova-3 streaming is $0.0077/min."})
    r = await configured(
        emitted,
        tools=tools,
        loop=loop_that('{"cards":[{"kind":"fact","text":"$0.0077/min","quote":"x"},{"kind":"fact","text":"two","quote":""}]}'),
    )
    r.ask("Nova-3 streaming price?", "What does streaming cost?")
    await settle()
    kinds = [m["type"] for m in emitted]
    assert kinds == ["research_started", "research_cards"]
    done = emitted[-1]
    assert done["cards"] == [{"kind": "fact", "text": "$0.0077/min", "quote": "What does streaming cost?"}], "one card, quoting the turn"
    assert done["sources"] == ["DeepHive"]
    assert tools.calls == [("search_knowledge", {"query": "q"})]
    await r.close()
    assert tools.closed == 1


async def test_one_lookup_at_a_time_and_only_the_newest_waits():
    emitted: list[dict] = []
    r = await configured(emitted, loop=loop_that('{"cards":[]}', delay=0.05))
    r.ask("first?", "a")
    r.ask("first?", "a")  # a repeat of the running question is ignored
    r.ask("second?", "b")
    r.ask("third?", "c")
    await asyncio.sleep(0.2)
    started = [m["question"] for m in emitted if m["type"] == "research_started"]
    assert started == ["first?", "third?"]
    await r.close()


async def test_failures_are_reported_not_raised():
    emitted: list[dict] = []

    async def boom(*_):
        raise RuntimeError("401 from provider")

    r = await configured(emitted, loop=boom)
    r.ask("q?", "t")
    await settle()
    assert emitted[-1] == {"type": "research_error", "research_id": 1, "question": "q?", "error": "401 from provider"}

    emitted.clear()
    r2 = await configured(emitted, tools=FakeTools(specs=[]))
    r2.ask("q?", "t")
    await settle()
    assert "no MCP tools" in emitted[-1]["error"]


async def test_a_slow_lookup_times_out():
    emitted: list[dict] = []
    r = Research(emitted.append)
    await r.configure(
        tools=FakeTools(), loop=loop_that("{}", delay=1), system_prompt="", limits=ResearchLimits(timeout_secs=0.05), config_key="k"
    )
    r.ask("q?", "t")
    await asyncio.sleep(0.2)
    assert "timed out" in emitted[-1]["error"]


async def test_tool_calls_are_capped():
    emitted: list[dict] = []
    tools = FakeTools()
    seen: list[str] = []

    async def greedy(system, question, specs, call_tool):
        for _ in range(6):
            seen.append(await call_tool("search_knowledge", {}))
        return '{"cards":[]}'

    r = Research(emitted.append)
    await r.configure(tools=tools, loop=greedy, system_prompt="", limits=ResearchLimits(max_tool_calls=2), config_key="k")
    r.ask("q?", "t")
    await settle()
    assert len(tools.calls) == 2
    assert seen[2].startswith("Error: tool call limit")


async def test_the_same_servers_keep_their_connection_new_ones_replace_it():
    emitted: list[dict] = []
    first = FakeTools()
    r = await configured(emitted, tools=first, key="same")
    again = FakeTools()
    await r.configure(tools=again, loop=loop_that("{}"), system_prompt="R2", limits=ResearchLimits(), config_key="same")
    assert again.closed == 1 and first.closed == 0, "the new duplicate is closed, the open one kept"
    other = FakeTools()
    await r.configure(tools=other, loop=loop_that("{}"), system_prompt="R3", limits=ResearchLimits(), config_key="other")
    assert first.closed == 1
    await r.configure(tools=None, loop=None, system_prompt="", limits=ResearchLimits(), config_key=None)
    assert other.closed == 1 and not r.enabled


async def test_a_server_that_fails_to_connect_is_reported_at_configure():
    emitted: list[dict] = []
    await configured(emitted, tools=FakeTools(fail_list="could not connect to MCP server X: boom"))
    await settle()
    assert emitted[-1]["type"] == "research_error" and "boom" in emitted[-1]["error"]


# ---- through Assist ---------------------------------------------------------


async def test_a_card_reply_with_a_question_starts_a_lookup_and_cards_still_arrive_first():
    emitted: list[dict] = []

    async def infer(system, prompt):
        return '{"cards":[{"kind":"ask","text":"Which volume?","quote":"cost"}],"research":"Nova-3 streaming price"}'

    a = Assist(emitted.append)
    a.configure(system_prompt="S", infer=infer, llm_error=None, enabled=True)
    await a.research.configure(
        tools=FakeTools(),
        loop=loop_that('{"cards":[{"kind":"fact","text":"$0.0077/min","quote":""}]}', delay=0.02),
        system_prompt="R",
        limits=ResearchLimits(),
        config_key="k",
    )
    a.on_end_of_turn("What does streaming cost?")
    await asyncio.sleep(0.1)
    kinds = [m["type"] for m in emitted]
    assert kinds.index("assist_cards") < kinds.index("research_started") < kinds.index("research_cards")
    card = next(m for m in emitted if m["type"] == "research_cards")["cards"][0]
    assert card["quote"] == "What does streaming cost?"
    await a.close()


async def test_turning_assist_off_cancels_the_lookup():
    emitted: list[dict] = []
    a = Assist(emitted.append)
    a.configure(system_prompt="S", infer=None, llm_error="x", enabled=True)
    await a.research.configure(
        tools=FakeTools(), loop=loop_that("{}", delay=1), system_prompt="R", limits=ResearchLimits(), config_key="k"
    )
    a.research.ask("q?", "t")
    await settle()
    a.set_enabled(False)
    await asyncio.sleep(0.05)
    assert [m["type"] for m in emitted] == ["research_started"], "no result after off"
    await a.close()


# ---- tool loops (provider SDK shapes, faked) --------------------------------


class FakeAnthropic:
    def __init__(self, replies, reject_thinking=False):
        self.replies = list(replies)
        self.requests: list[dict] = []
        self.reject_thinking = reject_thinking
        self.messages = self

    async def create(self, **params):
        self.requests.append(params)
        if self.reject_thinking and "thinking" in params:
            raise RuntimeError("400: thinking type between_tools is not supported")
        return self.replies.pop(0)


def block(**kw):
    return SimpleNamespace(**kw, model_dump=lambda exclude_none=True: dict(kw))


async def test_claude_loop_runs_tools_then_answers():
    client = FakeAnthropic(
        [
            SimpleNamespace(content=[block(type="tool_use", id="t1", name="search_knowledge", input={"query": "price"})]),
            SimpleNamespace(content=[block(type="text", text='{"cards":[]}')]),
        ]
    )
    loop = _claude_loop(client, "claude-sonnet-5-5", 512, 3)
    calls = []

    async def call_tool(name, args):
        calls.append((name, args))
        return "result text"

    spec = ToolSpec("search_knowledge", "Search", {"type": "object"}, "DeepHive")
    out = await loop("SYS", "price?", [spec], call_tool)
    assert out == '{"cards":[]}'
    assert calls == [("search_knowledge", {"query": "price"})]
    first, second = client.requests
    assert first["system"][0]["cache_control"] == {"type": "ephemeral"}
    assert first["thinking"] == {"type": "between_tools"}
    assert first["tools"][0]["name"] == "search_knowledge"
    assert second["messages"][-1]["content"][0] == {"type": "tool_result", "tool_use_id": "t1", "content": "result text"}


async def test_claude_loop_retries_without_thinking_and_forces_an_answer_on_the_last_round():
    use = lambda i: SimpleNamespace(content=[block(type="tool_use", id=f"t{i}", name="s", input={})])
    client = FakeAnthropic([use(1), SimpleNamespace(content=[block(type="text", text="done")])], reject_thinking=True)
    loop = _claude_loop(client, "m", 100, 1)

    async def call_tool(name, args):
        return "r"

    assert await loop("S", "q", [ToolSpec("s", "", {"type": "object"}, "X")], call_tool) == "done"
    assert "thinking" not in client.requests[-1]
    assert client.requests[-1]["tool_choice"] == {"type": "none"}
    from meetily_sidecar.llm import LAST_ROUND_NOTE

    assert client.requests[-1]["messages"][-1]["content"][-1] == {"type": "text", "text": LAST_ROUND_NOTE}, (
        "the model is told to answer: tool_choice none alone gave empty replies"
    )


async def test_openai_loop_runs_tools_then_answers():
    tool_call = SimpleNamespace(id="c1", function=SimpleNamespace(name="search_knowledge", arguments='{"query":"x"}'))
    msg1 = SimpleNamespace(tool_calls=[tool_call], content=None, model_dump=lambda exclude_none=True: {"role": "assistant", "tool_calls": []})
    msg2 = SimpleNamespace(tool_calls=None, content='{"cards":[]}')
    replies = [SimpleNamespace(choices=[SimpleNamespace(message=m)]) for m in (msg1, msg2)]
    requests = []

    async def create(**params):
        requests.append(params)
        return replies.pop(0)

    client = SimpleNamespace(chat=SimpleNamespace(completions=SimpleNamespace(create=create)))
    loop = _openai_loop(client, "gpt", 100, 3)
    calls = []

    async def call_tool(name, args):
        calls.append((name, args))
        return "r"

    assert await loop("S", "q", [ToolSpec("search_knowledge", "", {"type": "object"}, "X")], call_tool) == '{"cards":[]}'
    assert calls == [("search_knowledge", {"query": "x"})]
    assert requests[1]["messages"][-1] == {"role": "tool", "tool_call_id": "c1", "content": "r"}

    replies[:] = [SimpleNamespace(choices=[SimpleNamespace(message=m)]) for m in (msg1, msg2)]
    requests.clear()
    await _openai_loop(client, "gpt", 100, 1)("S", "q", [ToolSpec("search_knowledge", "", {"type": "object"}, "X")], call_tool)
    from meetily_sidecar.llm import LAST_ROUND_NOTE

    assert requests[1]["messages"][-1] == {"role": "user", "content": LAST_ROUND_NOTE}
    assert requests[1]["tool_choice"] == "none"


def test_make_tool_loop_rejects_unsupported_providers():
    from meetily_sidecar.llm import UnsupportedProvider

    with pytest.raises(UnsupportedProvider):
        make_tool_loop({"provider": "builtin-ai", "model": "m"}, 100, 2)
    with pytest.raises(UnsupportedProvider):
        make_tool_loop({"provider": "custom-openai", "model": "m"}, 100, 2)
    assert make_tool_loop({"provider": "claude", "model": "m", "api_key": "k"}, 100, 2)


# ---- a real stdio MCP server through Pipecat's MCPClient ---------------------

SERVER = textwrap.dedent(
    """
    from mcp.server.mcpserver import MCPServer
    mcp = MCPServer("t")

    @mcp.tool()
    def search_knowledge(query: str) -> str:
        "Search the knowledge base."
        return "answer for " + query

    @mcp.tool()
    def delete_everything() -> str:
        "Not allowed."
        return "deleted"

    mcp.run()
    """
)


async def test_mcp_tools_over_stdio_honour_the_allowlist(tmp_path):
    script = tmp_path / "server.py"
    script.write_text(SERVER)
    errors: list[str] = []
    tools = MCPTools(
        [{"name": "Test", "transport": "stdio", "command": sys.executable, "args": [str(script)], "allowed_tools": ["search_knowledge"]}],
        errors.append,
    )
    try:
        specs = await asyncio.wait_for(tools.list(), 30)
        assert [s.name for s in specs] == ["search_knowledge"], errors
        assert specs[0].input_schema["properties"]["query"]["type"] == "string"
        assert await tools.call("search_knowledge", {"query": "nova"}) == "answer for nova"
        assert (await tools.call("delete_everything", {})).startswith("Error: no tool")
    finally:
        await tools.close()
    bad = MCPTools([{"name": "Bad", "transport": "stdio", "command": "/nonexistent/x", "args": []}], errors.append)
    assert await bad.list() == []
    assert errors and "Bad" in errors[-1]
