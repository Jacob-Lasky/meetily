use serde::{Deserialize, Serialize};
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tauri::command;

/// Groq model information returned to frontend
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GroqModel {
    pub id: String,
    pub owned_by: Option<String>,
}

/// API response model from Groq (OpenAI-compatible format)
#[derive(Debug, Deserialize)]
struct GroqApiModel {
    id: String,
    owned_by: Option<String>,
    #[allow(dead_code)]
    object: String,
    /// Unix seconds, used to list the newest models first.
    #[serde(default)]
    created: i64,
}

/// API response wrapper from Groq
#[derive(Debug, Deserialize)]
struct GroqApiResponse {
    data: Vec<GroqApiModel>,
}

/// Cache entry for models
struct CacheEntry {
    /// The key the list was fetched with: another account sees other models.
    api_key: String,
    models: Vec<GroqModel>,
    fetched_at: Instant,
}

/// Global cache for Groq models (5 minute TTL)
static MODELS_CACHE: RwLock<Option<CacheEntry>> = RwLock::new(None);

/// Cache TTL in seconds
const CACHE_TTL_SECS: u64 = 300;

/// Shown when no API key is set or the fetch fails. With a key, the live
/// `/v1/models` list replaces this. These are Groq's production text models
/// as of 2026-10-16 (preview models are evaluation-only and can vanish).
/// The frontend has no copy of this list: it always asks this command.
const FALLBACK_MODELS: &[&str] = &[
    "openai/gpt-oss-120b",
    "llama-3.3-70b-versatile",
    "openai/gpt-oss-20b",
    "llama-3.1-8b-instant",
];

/// Get fallback models as GroqModel vec
fn get_fallback_models() -> Vec<GroqModel> {
    FALLBACK_MODELS
        .iter()
        .map(|id| GroqModel {
            id: id.to_string(),
            owned_by: None,
        })
        .collect()
}

/// Check if model is a chat-capable model. Groq also serves speech-to-text
/// (whisper), text-to-speech (orpheus) and safety classifiers (prompt-guard,
/// gpt-oss-safeguard) from the same `/v1/models` list.
fn is_chat_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    !id.contains("whisper")
        && !id.contains("embed")
        && !id.contains("guard")
        && !id.contains("tool-use")
        && !id.contains("orpheus")
        && !id.contains("tts")
}

/// Keep the summary-capable models, newest first.
fn select_chat_models(mut data: Vec<GroqApiModel>) -> Vec<GroqModel> {
    data.retain(|m| is_chat_model(&m.id));
    data.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    data.into_iter()
        .map(|m| GroqModel {
            id: m.id,
            owned_by: m.owned_by,
        })
        .collect()
}

/// Fetch Groq models from API
///
/// # Arguments
/// * `api_key` - Groq API key
///
/// # Returns
/// Vector of available models, or fallback models on error
#[command]
pub async fn get_groq_models(api_key: Option<String>) -> Result<Vec<GroqModel>, String> {
    // Return fallback if no API key provided
    let api_key = match api_key {
        Some(key) if !key.trim().is_empty() => key.trim().to_string(),
        _ => {
            log::info!("No Groq API key provided, returning fallback models");
            return Ok(get_fallback_models());
        }
    };

    // Check cache first
    {
        let cache = MODELS_CACHE.read().map_err(|e| e.to_string())?;
        if let Some(entry) = cache.as_ref() {
            if entry.api_key == api_key
                && entry.fetched_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS)
            {
                log::info!("Returning cached Groq models ({} models)", entry.models.len());
                return Ok(entry.models.clone());
            }
        }
    }

    // Fetch from API
    log::info!("Fetching Groq models from API...");
    let client = reqwest::Client::new();

    let response = match client
        .get("https://api.groq.com/openai/v1/models")
        .header("Authorization", format!("Bearer {}", api_key))
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            log::warn!("Failed to fetch Groq models: {}. Using fallback.", e);
            return Ok(get_fallback_models());
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        log::warn!(
            "Groq API returned status {}. Using fallback models.",
            status
        );
        return Ok(get_fallback_models());
    }

    let api_response: GroqApiResponse = match response.json().await {
        Ok(data) => data,
        Err(e) => {
            log::warn!("Failed to parse Groq response: {}. Using fallback.", e);
            return Ok(get_fallback_models());
        }
    };

    let models = select_chat_models(api_response.data);

    // If no models returned, use fallback
    if models.is_empty() {
        log::warn!("No chat models returned from Groq API. Using fallback.");
        return Ok(get_fallback_models());
    }

    log::info!("Fetched {} Groq models from API", models.len());

    // Update cache
    {
        let mut cache = MODELS_CACHE.write().map_err(|e| e.to_string())?;
        *cache = Some(CacheEntry {
            api_key,
            models: models.clone(),
            fetched_at: Instant::now(),
        });
    }

    Ok(models)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_speech_and_safety_models() {
        // Model ids from Groq's production and preview lists, 2026-10-16.
        for id in [
            "whisper-large-v3-turbo",
            "canopylabs/orpheus-v1-english",
            "meta-llama/llama-prompt-guard-2-86m",
            "openai/gpt-oss-safeguard-20b",
        ] {
            assert!(!is_chat_model(id), "{id} should not be listed");
        }
        for id in FALLBACK_MODELS.iter().chain(&["qwen/qwen3.8-27b", "minimaxai/minimax-m2.7"]) {
            assert!(is_chat_model(id), "{id} should be listed");
        }
    }

    #[test]
    fn sorts_newest_first() {
        let model = |id: &str, created| GroqApiModel {
            id: id.to_string(),
            owned_by: None,
            object: "model".to_string(),
            created,
        };
        let picked = select_chat_models(vec![
            model("llama-3.1-8b-instant", 1_693_721_698),
            model("whisper-large-v3", 1_693_721_698),
            model("openai/gpt-oss-120b", 1_754_408_224),
        ]);
        let ids: Vec<_> = picked.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["openai/gpt-oss-120b", "llama-3.1-8b-instant"]);
    }
}
