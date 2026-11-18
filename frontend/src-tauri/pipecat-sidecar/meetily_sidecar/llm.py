"""The assist call, on a Pipecat LLM service.

Rust resolves the configured summary provider, model and key (the same
`LlmTarget` the summaries use) and sends them in the assist config, so there
is no second key setting. Each call is a one-shot, out-of-pipeline
`run_inference` on the provider's Pipecat service.

Thinking on Claude: Pipecat 1.16.0 sends the lowest documented thinking
setting when none is given ({"type": "between_tools"} for claude-sonnet-5-5),
which is exactly what WORK-524 measured as the fast setting ({"type":
"disabled"} is a 400 on that model). Do not pass a thinking config here.

Prompt caching on Claude (WORK-581): the system prompt carries Me's
background and knowledge and is the same on every call of a meeting, so the
card service turns on `enable_prompt_caching` (Pipecat marks the system
prompt `cache_control: ephemeral`).

The research lane's tool loop (`make_tool_loop`) talks to the provider SDKs
directly: Pipecat's `run_inference` sends tools but does not run tool calls
outside a pipeline.
"""

from __future__ import annotations

import json
from typing import Any, Awaitable, Callable

from loguru import logger

from .assist import Infer
from .research import ToolLoop, ToolSpec

#: Default endpoints, matching `summary/llm_client.rs`.
OLLAMA_DEFAULT = "http://localhost:11434"

class UnsupportedProvider(ValueError):
    pass


def build_service(cfg: dict[str, Any]):
    """A Pipecat LLM service for `cfg` = {provider, model, api_key, endpoint?}."""
    provider = (cfg.get("provider") or "").lower()
    model = cfg.get("model") or ""
    key = cfg.get("api_key") or ""
    endpoint = cfg.get("endpoint")
    if not model:
        raise UnsupportedProvider("no summary model is configured (Settings > Summary)")
    if provider == "claude":
        from pipecat.services.anthropic.llm import AnthropicLLMService

        return AnthropicLLMService(
            api_key=key, settings=AnthropicLLMService.Settings(model=model, enable_prompt_caching=True)
        )
    if provider == "openai":
        from pipecat.services.openai.llm import OpenAILLMService

        return OpenAILLMService(api_key=key, settings=OpenAILLMService.Settings(model=model))
    if provider == "groq":
        from pipecat.services.groq.llm import GroqLLMService

        return GroqLLMService(api_key=key, settings=GroqLLMService.Settings(model=model))
    if provider == "openrouter":
        from pipecat.services.openrouter.llm import OpenRouterLLMService

        return OpenRouterLLMService(api_key=key, settings=OpenRouterLLMService.Settings(model=model))
    if provider == "ollama":
        from pipecat.services.ollama.llm import OLLamaLLMService

        base = (endpoint or OLLAMA_DEFAULT).rstrip("/")
        return OLLamaLLMService(base_url=f"{base}/v1", settings=OLLamaLLMService.Settings(model=model))
    if provider == "custom-openai":
        from pipecat.services.openai.llm import OpenAILLMService

        if not endpoint:
            raise UnsupportedProvider("the custom OpenAI provider has no endpoint configured")
        return OpenAILLMService(
            api_key=key or "none", base_url=endpoint, settings=OpenAILLMService.Settings(model=model)
        )
    raise UnsupportedProvider(
        f"live assist on the Pipecat engine cannot use the {provider or 'unset'} summary provider; "
        "pick Claude, OpenAI, Groq, OpenRouter, Ollama or a custom OpenAI endpoint in Settings > Summary"
    )


def make_infer(cfg: dict[str, Any], max_tokens: int) -> Infer:
    """An `Infer` coroutine bound to one service instance. `max_tokens` comes
    from Rust (`Limits`)."""
    from pipecat.processors.aggregators.llm_context import LLMContext

    service = build_service(cfg)

    async def infer(system_prompt: str, prompt: str) -> str:
        context = LLMContext(messages=[{"role": "user", "content": prompt}])
        out = await service.run_inference(context, max_tokens=max_tokens, system_instruction=system_prompt)
        return out or ""

    return infer


#: OpenAI-compatible base URLs, matching `summary/llm_client.rs`.
OPENAI_COMPAT_BASE = {
    "openai": "https://api.openai.com/v1",
    "groq": "https://api.groq.com/openai/v1",
    "openrouter": "https://openrouter.ai/api/v1",
}

#: The fast thinking setting (see the module docstring). Sent explicitly
#: here because this loop calls the SDK, not Pipecat.
CLAUDE_THINKING_FAST = {"type": "between_tools"}

CallTool = Callable[[str, dict], Awaitable[str]]

#: Added to the tool results before the last round. Measured 2026-11-18: with
#: tools merely switched off (tool_choice none), claude-sonnet-5-5 ended the
#: turn with an empty reply after its searches, so the lookup produced
#: nothing; told plainly, it answers.
LAST_ROUND_NOTE = "No more tool calls. Reply now with the JSON object, from what the results above say."


def make_tool_loop(cfg: dict[str, Any], max_tokens: int, max_tool_calls: int) -> ToolLoop:
    """The research lane's tool-use loop for `cfg` = {provider, model,
    api_key, endpoint?}: up to `max_tool_calls` rounds of tool calls, then a
    last round with tools off so the model has to answer."""
    provider = (cfg.get("provider") or "").lower()
    model = cfg.get("model") or ""
    key = cfg.get("api_key") or ""
    endpoint = cfg.get("endpoint")
    if not model:
        raise UnsupportedProvider("no assist model is configured")
    if provider == "claude":
        from anthropic import AsyncAnthropic

        return _claude_loop(AsyncAnthropic(api_key=key), model, max_tokens, max_tool_calls)
    if provider in OPENAI_COMPAT_BASE or provider in ("ollama", "custom-openai"):
        from openai import AsyncOpenAI

        if provider == "ollama":
            base = f"{(endpoint or OLLAMA_DEFAULT).rstrip('/')}/v1"
        elif provider == "custom-openai":
            if not endpoint:
                raise UnsupportedProvider("the custom OpenAI provider has no endpoint configured")
            base = endpoint
        else:
            base = OPENAI_COMPAT_BASE[provider]
        client = AsyncOpenAI(api_key=key or "none", base_url=base)
        return _openai_loop(client, model, max_tokens, max_tool_calls)
    raise UnsupportedProvider(f"research cannot use the {provider or 'unset'} provider")


def _claude_loop(client, model: str, max_tokens: int, max_tool_calls: int) -> ToolLoop:
    async def loop(system: str, question: str, tools: list[ToolSpec], call_tool: CallTool) -> str:
        messages: list[dict] = [{"role": "user", "content": f"Question: {question}"}]
        specs = [{"name": t.name, "description": t.description, "input_schema": t.input_schema} for t in tools]
        system_blocks = [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]
        thinking: dict | None = CLAUDE_THINKING_FAST
        for round_no in range(max_tool_calls + 1):
            last = round_no == max_tool_calls
            params: dict[str, Any] = {
                "model": model,
                "max_tokens": max_tokens,
                "system": system_blocks,
                "messages": messages,
                "tools": specs,
                "tool_choice": {"type": "none"} if last else {"type": "auto"},
            }
            if thinking is not None:
                params["thinking"] = thinking
            try:
                response = await client.messages.create(**params)
            except Exception as e:  # noqa: BLE001 - one retry for a model that rejects the setting
                if thinking is not None and "thinking" in str(e).lower():
                    logger.warning("research: Claude rejected the thinking setting; retrying without it")
                    thinking = None
                    params.pop("thinking")
                    response = await client.messages.create(**params)
                else:
                    raise
            uses = [b for b in response.content if getattr(b, "type", None) == "tool_use"]
            if not uses:
                return "".join(getattr(b, "text", "") or "" for b in response.content if b.type == "text")
            # Thinking blocks must go back unchanged with the tool results.
            messages.append({"role": "assistant", "content": [b.model_dump(exclude_none=True) for b in response.content]})
            results: list[dict] = []
            for use in uses:
                text = await call_tool(use.name, dict(use.input or {}))
                results.append({"type": "tool_result", "tool_use_id": use.id, "content": text})
            if round_no + 1 == max_tool_calls:
                results.append({"type": "text", "text": LAST_ROUND_NOTE})
            messages.append({"role": "user", "content": results})
        return ""

    return loop


def _openai_loop(client, model: str, max_tokens: int, max_tool_calls: int) -> ToolLoop:
    async def loop(system: str, question: str, tools: list[ToolSpec], call_tool: CallTool) -> str:
        messages: list[dict] = [
            {"role": "system", "content": system},
            {"role": "user", "content": f"Question: {question}"},
        ]
        specs = [
            {"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}}
            for t in tools
        ]
        for round_no in range(max_tool_calls + 1):
            last = round_no == max_tool_calls
            response = await client.chat.completions.create(
                model=model,
                messages=messages,
                tools=specs,
                tool_choice="none" if last else "auto",
                max_completion_tokens=max_tokens,
            )
            message = response.choices[0].message
            calls = message.tool_calls or []
            if not calls:
                return message.content or ""
            messages.append(message.model_dump(exclude_none=True))
            for call in calls:
                try:
                    args = json.loads(call.function.arguments or "{}")
                except ValueError:
                    args = {}
                text = await call_tool(call.function.name, args if isinstance(args, dict) else {})
                messages.append({"role": "tool", "tool_call_id": call.id, "content": text})
            if round_no + 1 == max_tool_calls:
                messages.append({"role": "user", "content": LAST_ROUND_NOTE})
        return ""

    return loop
