use serde::{Deserialize, Serialize};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tauri::command;

/// OpenAI model information returned to frontend
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAIModel {
    pub id: String,
}

/// API response model from OpenAI
#[derive(Debug, Deserialize)]
struct OpenAIApiModel {
    id: String,
    #[allow(dead_code)]
    object: String,
    #[allow(dead_code)]
    owned_by: String,
    /// Unix seconds. `/v1/models` returns models in no useful order, so this
    /// is what puts the newest models at the top of the picker.
    #[serde(default)]
    created: i64,
}

/// API response wrapper from OpenAI
#[derive(Debug, Deserialize)]
struct OpenAIApiResponse {
    data: Vec<OpenAIApiModel>,
}

/// Cache entry for models
struct CacheEntry {
    models: Vec<OpenAIModel>,
    fetched_at: Instant,
}

/// Global cache for OpenAI models (5 minute TTL)
static MODELS_CACHE: RwLock<Option<CacheEntry>> = RwLock::new(None);

/// Cache TTL in seconds
const CACHE_TTL_SECS: u64 = 300;

/// Shown when no API key is set or the fetch fails. With a key, the live
/// `/v1/models` list replaces this, so it only needs OpenAI's current
/// recommended lineup (flagship, balanced, high-volume), newest first.
/// The frontend has no copy of this list: it always asks this command.
const FALLBACK_MODELS: &[&str] = &["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"];

/// Get fallback models as OpenAIModel vec
fn get_fallback_models() -> Vec<OpenAIModel> {
    FALLBACK_MODELS
        .iter()
        .map(|id| OpenAIModel { id: id.to_string() })
        .collect()
}

/// Substrings marking models that cannot summarize through
/// `/v1/chat/completions`, which is the only endpoint Meetily calls for OpenAI.
/// Measured against the live API on 2026-10-16: `-pro` and `codex` models are
/// Responses-only (404 "not a chat model"), `deep-research`, `search-preview`
/// and the dated `*-chat-latest` aliases are still listed by `/v1/models` but
/// 404 as deprecated, and the rest are not text chat models at all.
/// DO NOT drop an entry without re-probing chat/completions for that family.
const NON_CHAT_MARKERS: &[&str] = &[
    "embedding",
    "tts",
    "whisper",
    "dall-e",
    "babbage",
    "davinci",
    "instruct",
    "realtime",
    "audio",
    "transcribe",
    "image",
    "search",
    "codex",
    "deep-research",
    "computer-use",
    "moderation",
    "-pro",
    "gpt-live",
    "chat-latest",
];

/// Check if a model can be used for summaries via chat completions.
fn is_chat_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    // o-series ids are both bare ("o3") and suffixed ("o3-mini"); fine-tunes
    // ("ft:...") are account-specific and deliberately not listed.
    let is_o_series = ["o1", "o3", "o4"]
        .iter()
        .any(|p| id == *p || id.starts_with(&format!("{p}-")));
    (id.starts_with("gpt-") || is_o_series)
        && !NON_CHAT_MARKERS.iter().any(|m| id.contains(m))
}

/// Keep the summary-capable models, newest first.
fn select_chat_models(mut data: Vec<OpenAIApiModel>) -> Vec<OpenAIModel> {
    data.retain(|m| is_chat_model(&m.id));
    data.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    data.into_iter().map(|m| OpenAIModel { id: m.id }).collect()
}

/// Fetch OpenAI models from API
///
/// # Arguments
/// * `api_key` - OpenAI API key
///
/// # Returns
/// Vector of available models, or fallback models on error
#[command]
pub async fn get_openai_models(api_key: Option<String>) -> Result<Vec<OpenAIModel>, String> {
    // Return fallback if no API key provided
    let api_key = match api_key {
        Some(key) if !key.trim().is_empty() => key.trim().to_string(),
        _ => {
            log::info!("No OpenAI API key provided, returning fallback models");
            return Ok(get_fallback_models());
        }
    };

    // Check cache first
    {
        let cache = MODELS_CACHE.read().map_err(|e| e.to_string())?;
        if let Some(entry) = cache.as_ref() {
            if entry.fetched_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS) {
                log::info!("Returning cached OpenAI models ({} models)", entry.models.len());
                return Ok(entry.models.clone());
            }
        }
    }

    // Fetch from API
    log::info!("Fetching OpenAI models from API...");
    let client = reqwest::Client::new();

    let response = match client
        .get("https://api.openai.com/v1/models")
        .header("Authorization", format!("Bearer {}", api_key))
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            log::warn!("Failed to fetch OpenAI models: {}. Using fallback.", e);
            return Ok(get_fallback_models());
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        log::warn!(
            "OpenAI API returned status {}. Using fallback models.",
            status
        );
        return Ok(get_fallback_models());
    }

    let api_response: OpenAIApiResponse = match response.json().await {
        Ok(data) => data,
        Err(e) => {
            log::warn!("Failed to parse OpenAI response: {}. Using fallback.", e);
            return Ok(get_fallback_models());
        }
    };

    let models = select_chat_models(api_response.data);

    // If no models returned (e.g., restricted API key), use fallback
    if models.is_empty() {
        log::warn!("No chat models returned from OpenAI API. Using fallback.");
        return Ok(get_fallback_models());
    }

    log::info!("Fetched {} OpenAI models from API", models.len());

    // Update cache
    {
        let mut cache = MODELS_CACHE.write().map_err(|e| e.to_string())?;
        *cache = Some(CacheEntry {
            models: models.clone(),
            fetched_at: Instant::now(),
        });
    }

    Ok(models)
}

/// Clear the models cache (useful when API key changes)
pub fn clear_cache() {
    if let Ok(mut cache) = MODELS_CACHE.write() {
        *cache = None;
        log::info!("OpenAI models cache cleared");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_model(id: &str, created: i64) -> OpenAIApiModel {
        OpenAIApiModel {
            id: id.to_string(),
            object: "model".to_string(),
            owned_by: "system".to_string(),
            created,
        }
    }

    #[test]
    fn keeps_models_that_answer_chat_completions() {
        // Each returned 200 from /v1/chat/completions on 2026-10-16.
        for id in [
            "gpt-6-astra",
            "gpt-6.1-sol",
            "gpt-6-luna",
            "gpt-5.5",
            "gpt-5.4-mini",
            "gpt-4.1-nano-2025-04-14",
            "gpt-4o",
            "o1",
            "o3",
            "o3-mini",
            "o4-mini-2025-04-16",
        ] {
            assert!(is_chat_model(id), "{id} should be listed");
        }
    }

    #[test]
    fn drops_models_that_fail_chat_completions() {
        // Each returned 404 from /v1/chat/completions on 2026-10-16, or is not
        // a text chat model at all.
        for id in [
            "gpt-5.5-pro",
            "o3-pro-2025-06-10",
            "o1-pro",
            "gpt-5.3-codex",
            "o3-deep-research",
            "gpt-4o-search-preview",
            "gpt-5-search-api",
            "gpt-5.2-chat-latest",
            "gpt-realtime-2.1",
            "gpt-live-1",
            "gpt-transcribe",
            "gpt-image-2",
            "gpt-4o-mini-tts",
            "text-embedding-3-large",
            "omni-moderation-latest",
            "ft:gpt-4.1-mini-2025-04-14:org:name:abc",
        ] {
            assert!(!is_chat_model(id), "{id} should not be listed");
        }
    }

    #[test]
    fn sorts_newest_first() {
        let picked = select_chat_models(vec![
            api_model("gpt-3.5-turbo", 1_677_610_602),
            api_model("gpt-6-astra", 1_787_853_604),
            api_model("gpt-image-2", 1_776_399_795),
            api_model("gpt-6.1-sol", 1_790_552_874),
        ]);
        let ids: Vec<_> = picked.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6.1-sol", "gpt-6-astra", "gpt-3.5-turbo"]);
    }

    #[test]
    fn fallback_models_pass_the_chat_filter() {
        for id in FALLBACK_MODELS {
            assert!(is_chat_model(id), "fallback {id} would be filtered out live");
        }
    }
}
