"""The research lane (WORK-581): tool-using lookups beside the fast cards.

The fast card call stays tool-free, so it keeps its latency. When it decides
Them asked for a fact that neither the transcript nor Me's background holds,
its JSON carries `"research": "<question>"`; `Assist` hands that here. A
lookup is a separate, slower call that may use the MCP servers' tools (Pipecat
`MCPClient`: stdio, SSE or streamable HTTP), and its answer arrives as a
follow-up card.

- One lookup runs at a time. A question asked while one runs waits; only the
  newest waiting question is kept (the conversation has moved on).
- MCP connections open when the lane is configured, so the first lookup does
  not pay for a server start (a stdio relay may first fetch a token).
- Nothing here logs a question, a tool argument or a tool result: they are
  built from what was said in the meeting, and these lines end up in
  Meetily's log.

Pure asyncio around two injected pieces, so tests drive it with fakes:
`Tools` (list and call) and `ToolLoop` (the model's tool-use loop).
"""

from __future__ import annotations

import asyncio
import math
import re
import time
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Protocol

from loguru import logger

from .prompt import ParseError, parse_cards

Emit = Callable[[dict], None]


@dataclass(frozen=True)
class ToolSpec:
    """A tool as the model sees it. `name` is unique across servers."""

    name: str
    description: str
    input_schema: dict
    server: str


class Tools(Protocol):
    async def list(self) -> list[ToolSpec]: ...

    async def call(self, name: str, arguments: dict) -> str: ...

    async def close(self) -> None: ...


#: `(system_prompt, question, tools, call_tool) -> final text`. `call_tool`
#: runs one tool by name and returns its text; the loop decides when to stop.
ToolLoop = Callable[
    [str, str, list[ToolSpec], Callable[[str, dict], Awaitable[str]]], Awaitable[str]
]


@dataclass(frozen=True)
class ResearchLimits:
    """Their one home is Rust (`live_assist`: RESEARCH_TIMEOUT,
    RESEARCH_MAX_TOOL_CALLS, CLAUDE_MAX_TOKENS), sent in the assist config."""

    timeout_secs: float = 45.0
    max_tool_calls: int = 4
    max_tokens: int = 1024

    @classmethod
    def from_config(cls, raw: Any) -> "ResearchLimits":
        base = cls()
        if not isinstance(raw, dict):
            return base
        timeout = raw.get("timeout_secs")
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not (
            math.isfinite(timeout) and timeout > 0
        ):
            timeout = base.timeout_secs

        def count(name):
            value = raw.get(name)
            if isinstance(value, bool) or not isinstance(value, int) or value < 1:
                return getattr(base, name)
            return value

        return cls(timeout_secs=float(timeout), max_tool_calls=count("max_tool_calls"), max_tokens=count("max_tokens"))


class MCPTools:
    """Tools of several MCP servers through Pipecat's `MCPClient`.

    A server that fails to connect is reported once and left out; the others
    still serve. Tool names that two servers share are prefixed with the
    server name for the model, and mapped back for the call.
    """

    def __init__(self, servers: list[dict], on_error: Callable[[str], None]):
        self._servers = servers
        self._on_error = on_error
        self._clients: dict[str, Any] = {}
        self._route: dict[str, tuple[str, str]] = {}  # model name -> (server, tool)
        self._specs: list[ToolSpec] | None = None
        self._lock = asyncio.Lock()
        self._closed = False

    @staticmethod
    def server_params(cfg: dict):
        transport = cfg.get("transport")
        if transport == "stdio":
            from mcp import StdioServerParameters

            env = cfg.get("env") or None
            if env is not None:
                # The SDK's default environment (HOME, PATH, USER, ...) plus
                # the extra variables: an `env` replaces the default outright.
                from mcp.client.stdio import get_default_environment

                env = {**get_default_environment(), **env}
            return StdioServerParameters(command=cfg["command"], args=list(cfg.get("args") or []), env=env)
        if transport == "sse":
            from mcp.client.session_group import SseServerParameters

            return SseServerParameters(url=cfg["url"], headers=dict(cfg.get("headers") or {}))
        if transport == "http":
            from mcp.client.session_group import StreamableHttpParameters

            return StreamableHttpParameters(url=cfg["url"], headers=dict(cfg.get("headers") or {}))
        raise ValueError(f"unknown MCP transport {transport!r}")

    async def list(self) -> list[ToolSpec]:
        async with self._lock:
            if self._specs is not None:
                return self._specs
            from pipecat.services.mcp_service import MCPClient

            specs: list[ToolSpec] = []
            seen: set[str] = set()
            for cfg in self._servers:
                name = str(cfg.get("name") or "MCP")
                allowed = [t for t in (cfg.get("allowed_tools") or []) if isinstance(t, str)] or None
                client = None
                try:
                    client = MCPClient(server_params=self.server_params(cfg), tools_filter=allowed)
                    schema = await client.tools()
                except Exception as e:  # noqa: BLE001 - one bad server must not stop the others
                    self._on_error(f"could not connect to MCP server {name}: {e}")
                    if client is not None:
                        await _close_quietly(client)
                    continue
                if self._closed:
                    # Closed while this server was connecting: nobody owns it.
                    await _close_quietly(client)
                    return []
                self._clients[name] = client
                for fn in schema.standard_tools:
                    model_name = tool_name_for(fn.name, name, seen)
                    seen.add(model_name)
                    self._route[model_name] = (name, fn.name)
                    specs.append(
                        ToolSpec(
                            name=model_name,
                            description=fn.description or "",
                            input_schema={
                                "type": "object",
                                "properties": dict(fn.properties or {}),
                                "required": list(fn.required or []),
                            },
                            server=name,
                        )
                    )
                logger.info(f"research: MCP server {name} ready with {len(schema.standard_tools)} tool(s)")
            self._specs = specs
            return specs

    async def call(self, name: str, arguments: dict) -> str:
        route = self._route.get(name)
        if route is None:
            return f"Error: no tool named {name}"
        server, tool = route
        client = self._clients[server]
        # `_call_tool_text` is the call MCPClient's own handler makes (fixed
        # arguments, output filters, a dropped session reconnecting); the
        # public path needs a running pipeline's FunctionCallParams.
        return await client._call_tool_text(tool, arguments or {})

    async def close(self) -> None:
        # Set first: a connect still in progress closes its own client.
        self._closed = True
        clients = list(self._clients.values())
        self._clients.clear()
        for client in clients:
            await _close_quietly(client)


async def _close_quietly(client) -> None:
    try:
        await client.close()
    except Exception as e:  # noqa: BLE001
        logger.warning(f"research: closing an MCP client failed: {e}")


#: What provider tool APIs accept as a function name.
_TOOL_NAME_BAD = re.compile(r"[^A-Za-z0-9_-]")
_TOOL_NAME_MAX = 64


def tool_name_for(tool: str, server: str, taken: set[str]) -> str:
    """A provider-valid, unique function name for `tool` of `server`:
    [A-Za-z0-9_-], at most 64 characters; a name already taken (two servers
    with a `search`) gets the server's name in front, then a number."""
    base = _TOOL_NAME_BAD.sub("_", tool)[:_TOOL_NAME_MAX] or "tool"
    if base not in taken:
        return base
    prefixed = f"{_TOOL_NAME_BAD.sub('_', server)}__{base}"[:_TOOL_NAME_MAX]
    candidate, n = prefixed, 2
    while candidate in taken:
        suffix = f"_{n}"
        candidate = prefixed[: _TOOL_NAME_MAX - len(suffix)] + suffix
        n += 1
    return candidate


#: How long session end waits for a connecting MCP server to finish and be
#: closed.
WARM_UP_CLOSE_WAIT_SECS = 5.0


@dataclass
class _Lookup:
    research_id: int
    question: str
    quote: str
    started_at: float
    task: asyncio.Task | None = None
    sources: set[str] = field(default_factory=set)


class Research:
    def __init__(self, emit: Emit, *, clock: Callable[[], float] = time.monotonic):
        self._emit = emit
        self._clock = clock
        self._tools: Tools | None = None
        self._loop: ToolLoop | None = None
        self._system_prompt = ""
        self.limits = ResearchLimits()
        self._current: _Lookup | None = None
        self._waiting: tuple[str, str] | None = None
        self._next_id = 0
        self._warm: asyncio.Task | None = None
        self._config_key: Any = None
        self._configure_lock = asyncio.Lock()
        #: Bumped by close(): a config queued before it is stale and opens nothing.
        self.epoch = 0
        self.lookups_started = 0

    @property
    def enabled(self) -> bool:
        return self._tools is not None and self._loop is not None

    @property
    def has_tools(self) -> bool:
        return self._tools is not None

    def set_prompt(self, system_prompt: str, limits: ResearchLimits):
        self._system_prompt = system_prompt
        self.limits = limits

    async def configure(
        self,
        *,
        tools: Tools | None,
        loop: ToolLoop | None,
        system_prompt: str,
        limits: ResearchLimits,
        config_key: Any = None,
        epoch: int | None = None,
    ):
        """Swap in a new tool source and loop. The same `config_key` as last
        time keeps the open MCP connections (a re-sent config after a brief
        edit must not restart a stdio server). Serialized: configs apply in
        the order they arrived."""
        async with self._configure_lock:
            if epoch is not None and epoch != self.epoch:
                # Queued before the session ended (control disconnected).
                if tools is not None:
                    await tools.close()
                return
            await self._configure(tools, loop, system_prompt, limits, config_key)

    async def _configure(self, tools, loop, system_prompt, limits, config_key):
        self._system_prompt = system_prompt
        self.limits = limits
        self._loop = loop
        if config_key is not None and config_key == self._config_key and self._tools is not None:
            if tools is not None:
                await tools.close()
            return
        old = self._tools
        self._tools, self._config_key = tools, config_key
        self._cancel_current("research reconfigured")
        self._waiting = None
        if old is not None:
            await old.close()
        if tools is not None:
            # Connect now so the first lookup does not wait on a server start.
            self._warm = asyncio.get_running_loop().create_task(self._warm_up(tools))

    async def _warm_up(self, tools: Tools):
        try:
            await tools.list()
        except Exception as e:  # noqa: BLE001
            self._emit({"type": "research_error", "research_id": None, "question": None, "error": str(e)})

    def ask(self, question: str, quote: str):
        """Look up `question` (the card call's), tied to the transcript words
        `quote` so the card maps to a transcript line like any other."""
        question = " ".join(question.split())
        if not self.enabled or not question:
            return
        if self._current is not None:
            if self._current.question == question:
                return
            self._waiting = (question, quote)
            return
        self._start(question, quote)

    def cancel(self, reason: str):
        self._waiting = None
        self._cancel_current(reason)

    def _start(self, question: str, quote: str):
        self._next_id += 1
        self.lookups_started += 1
        lookup = _Lookup(self._next_id, question, quote, self._clock())
        self._current = lookup
        logger.info(f"research: lookup {lookup.research_id} started")
        self._emit({"type": "research_started", "research_id": lookup.research_id, "question": question})
        lookup.task = asyncio.get_running_loop().create_task(self._run(lookup))

    async def _run(self, lookup: _Lookup):
        assert self._tools is not None and self._loop is not None
        tools, loop = self._tools, self._loop
        calls = 0

        async def call_tool(name: str, arguments: dict) -> str:
            nonlocal calls
            calls += 1
            if calls > self.limits.max_tool_calls:
                return "Error: tool call limit reached; answer from what you have."
            spec = next((t for t in specs if t.name == name), None)
            if spec is not None:
                lookup.sources.add(spec.server)
            return await tools.call(name, arguments)

        error = None
        output = ""
        try:
            specs = await tools.list()
            if not specs:
                raise RuntimeError("no MCP tools are available (check Settings › AI Assist)")
            output = await asyncio.wait_for(
                loop(self._system_prompt, lookup.question, specs, call_tool), self.limits.timeout_secs
            )
        except asyncio.CancelledError:
            raise
        except asyncio.TimeoutError:
            error = f"lookup timed out after {self.limits.timeout_secs:g} s"
        except Exception as e:  # noqa: BLE001 - any provider or tool failure is this lookup's error
            error = str(e) or type(e).__name__
        call_ms = round((self._clock() - lookup.started_at) * 1000)
        if self._current is not lookup:
            return
        self._current = None
        if error is not None:
            logger.warning(f"research: lookup {lookup.research_id} failed after {call_ms} ms: {error}")
            self._emit(
                {"type": "research_error", "research_id": lookup.research_id, "question": lookup.question, "error": error}
            )
        else:
            try:
                cards = parse_cards(output)
            except ParseError as e:
                logger.warning(f"research: lookup {lookup.research_id}: {e} ({len(output)} chars)")
                cards = []
            logger.info(
                f"research: lookup {lookup.research_id} returned {len(cards)} card(s) in {call_ms} ms "
                f"after {calls} tool call(s)"
            )
            self._emit(
                {
                    "type": "research_cards",
                    "research_id": lookup.research_id,
                    "question": lookup.question,
                    # The card answers the turn that raised the question.
                    "cards": [{**c.as_json(), "quote": lookup.quote} for c in cards[:1]],
                    "sources": sorted(lookup.sources),
                    "call_ms": call_ms,
                }
            )
        waiting, self._waiting = self._waiting, None
        if waiting is not None and self.enabled:
            self._start(*waiting)

    def _cancel_current(self, reason: str):
        lookup, self._current = self._current, None
        if lookup is None:
            return
        if lookup.task is not None and not lookup.task.done():
            lookup.task.cancel()
        logger.info(f"research: lookup {lookup.research_id} cancelled ({reason})")

    async def close(self):
        self.epoch += 1
        self._waiting = None
        lookup = self._current
        self._cancel_current("session ended")
        if lookup is not None and lookup.task is not None:
            try:
                await lookup.task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        # A warm-up still connecting is not cancelled (that could strand the
        # MCP server it is starting): the tool source is closed below, and
        # the warm-up, seeing that, closes its own client when it connects.
        # Wait a little for that, so the sidecar does not exit first and
        # cancel it before the client is closed.
        warm = self._warm
        if self._tools is not None:
            await self._tools.close()
            self._tools = None
            self._config_key = None
        if warm is not None and not warm.done():
            try:
                await asyncio.wait_for(asyncio.shield(warm), WARM_UP_CLOSE_WAIT_SECS)
            except (asyncio.TimeoutError, asyncio.CancelledError, Exception):  # noqa: BLE001
                logger.warning("research: an MCP server was still connecting at shutdown")
