"""The assist call, on a Pipecat LLM service.

Rust resolves the configured summary provider, model and key (the same
`LlmTarget` the summaries use) and sends them in the assist config, so there
is no second key setting. Each call is a one-shot, out-of-pipeline
`run_inference` on the provider's Pipecat service.

Thinking on Claude: Pipecat 1.16.0 sends the lowest documented thinking
setting when none is given ({"type": "between_tools"} for claude-sonnet-5-5),
which is exactly what WORK-524 measured as the fast setting ({"type":
"disabled"} is a 400 on that model). Do not pass a thinking config here.
"""

from __future__ import annotations

from typing import Any

from .assist import Infer

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

        return AnthropicLLMService(api_key=key, settings=AnthropicLLMService.Settings(model=model))
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
