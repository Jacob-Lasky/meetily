//! Where an LLM call goes: provider, credentials and endpoint settings, read
//! from the settings table. Shared by summaries (`service.rs`) and live
//! assist so both apply the same key and endpoint rules.

use log::info;
use sqlx::SqlitePool;

use crate::database::repositories::setting::SettingsRepository;
use crate::summary::llm_client::LLMProvider;

#[derive(Debug, Clone)]
pub struct LlmTarget {
    pub provider: LLMProvider,
    /// Empty for providers without a key (Ollama, Built-in AI, a keyless
    /// custom endpoint).
    pub api_key: String,
    pub ollama_endpoint: Option<String>,
    pub custom_openai_endpoint: Option<String>,
    pub custom_openai_max_tokens: Option<u32>,
    pub custom_openai_temperature: Option<f32>,
    pub custom_openai_top_p: Option<f32>,
}

impl LlmTarget {
    /// Resolve `provider_name` ("claude", "openai", ...) against the stored
    /// settings. The error strings are shown to the user as-is.
    pub async fn resolve(pool: &SqlitePool, provider_name: &str) -> Result<Self, String> {
        let provider = LLMProvider::from_str(provider_name)?;

        // Ollama, Built-in AI and Custom OpenAI do not use the per-provider
        // key columns.
        let api_key = if matches!(
            provider,
            LLMProvider::Ollama | LLMProvider::BuiltInAI | LLMProvider::CustomOpenAI
        ) {
            String::new()
        } else {
            match SettingsRepository::get_api_key(pool, provider_name).await {
                Ok(Some(key)) if !key.is_empty() => key,
                Ok(None) | Ok(Some(_)) => return Err(format!("API key not found for {}", provider_name)),
                Err(e) => return Err(format!("Failed to retrieve API key for {}: {}", provider_name, e)),
            }
        };

        let ollama_endpoint = if provider == LLMProvider::Ollama {
            match SettingsRepository::get_model_config(pool).await {
                Ok(Some(config)) => config.ollama_endpoint,
                Ok(None) => None,
                Err(e) => {
                    info!("Failed to retrieve Ollama endpoint: {}, using default", e);
                    None
                }
            }
        } else {
            None
        };

        let mut target = Self {
            provider,
            api_key,
            ollama_endpoint,
            custom_openai_endpoint: None,
            custom_openai_max_tokens: None,
            custom_openai_temperature: None,
            custom_openai_top_p: None,
        };

        if target.provider == LLMProvider::CustomOpenAI {
            let config = match SettingsRepository::get_custom_openai_config(pool).await {
                Ok(Some(config)) => config,
                Ok(None) => return Err("Custom OpenAI provider selected but no configuration found".to_string()),
                Err(e) => return Err(format!("Failed to retrieve custom OpenAI config: {}", e)),
            };
            info!(
                "✓ Using custom OpenAI endpoint_origin={}",
                crate::utils::url_origin_for_log(&config.endpoint)
            );
            target.api_key = config.api_key.unwrap_or_default();
            target.custom_openai_endpoint = Some(config.endpoint);
            target.custom_openai_max_tokens = config.max_tokens.map(|t| t as u32);
            target.custom_openai_temperature = config.temperature;
            target.custom_openai_top_p = config.top_p;
        }

        Ok(target)
    }
}
