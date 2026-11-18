// audio/deepgram.rs
//
// First-class Deepgram transcription provider.
//
// Deepgram is a streaming-first cloud ASR: live recording uses the realtime
// WebSocket API (low-latency interim + final results), while imported files and
// retranscription use the prerecorded REST API. This does NOT map onto the
// batch `TranscriptionProvider` trait (chunk -> Vec<f32> -> text), so Deepgram
// is wired in as its own streaming path and the trait-based engine is bypassed
// for this provider.
//
// Wire contract (verified live against api.deepgram.com, nova-3):
//   - Auth header is `Authorization: Token <key>` (NOT `Bearer`).
//   - Prerecorded: a single synchronous POST of the audio bytes; the response
//     carries `results.utterances[]` (speaker-segmented turns) and
//     `results.channels[0].alternatives[0].{transcript,words}`. No polling.
//   - Realtime: params are passed as WS query string (not a config frame). Each
//     `Results` message carries `is_final`/`speech_final` and a smart-formatted
//     `channel.alternatives[0].transcript` plus word-level timing.

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};

use crate::api::TranscriptSegment;
use crate::database::repositories::setting::SettingsRepository;
use crate::state::AppState;

use super::audio_processing::{deinterleave, interleave_stereo};
use super::recording_state::{AudioChunk, AudioLayout};
use super::transcription::TranscriptUpdate;

pub const PROVIDER: &str = "deepgram";
const LISTEN_REST_URL: &str = "https://api.deepgram.com/v1/listen";
const LISTEN_WS_URL: &str = "wss://api.deepgram.com/v1/listen";
// Flux uses a separate v2 realtime endpoint with turn-based events.
const FLUX_WS_URL: &str = "wss://api.deepgram.com/v2/listen";
const PCM_SAMPLE_RATE: u32 = 16_000;
/// Deepgram closes an idle realtime socket after ~10s; send KeepAlive well under that.
const KEEPALIVE_SECS: u64 = 5;
/// Flux end-of-stream: 20 x 100 ms of silence (2 s) lets the last turn close.
const FLUX_SILENCE_FRAME_SAMPLES: usize = (PCM_SAMPLE_RATE / 10) as usize;
const FLUX_TRAILING_SILENCE_FRAMES: usize = 20;

static REALTIME_SEQUENCE_COUNTER: AtomicU64 = AtomicU64::new(0);
static REALTIME_SPEECH_DETECTED_EMITTED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// OPTIONS
// ============================================================================

/// Provider-specific knobs surfaced in Settings (model + keyterm prompting +
/// diarization). These are what make Deepgram worth wiring first-class rather
/// than flattening to a generic "text out" contract.
#[derive(Debug, Clone)]
pub struct DeepgramOptions {
    pub model: String,
    pub keyterms: Vec<String>,
    pub diarize: bool,
    /// Opt out of Deepgram's Model Improvement Program (sends `mip_opt_out=true`).
    /// Defaults to true: Meetily is privacy-first, so audio is not retained for
    /// model training unless the user explicitly opts back in.
    pub mip_opt_out: bool,
    /// Deepgram-specific language mode (`multi`, `detect`, or an ISO code). Kept
    /// separate from the shared global language pref so Deepgram-only values never
    /// leak into the Whisper/Parakeet path. None -> fall back to the caller's hint.
    pub language: Option<String>,
}

impl Default for DeepgramOptions {
    fn default() -> Self {
        Self {
            model: crate::config::DEFAULT_DEEPGRAM_REALTIME_MODEL.to_string(),
            keyterms: Vec::new(),
            // Opt-in: the Mic-In / Speaker-Out channels already separate the
            // user from everyone else, and diarization is billed as an add-on.
            diarize: false,
            mip_opt_out: true,
            language: None,
        }
    }
}

// ============================================================================
// RESPONSE TYPES
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct DeepgramWord {
    #[serde(default)]
    pub word: String,
    #[serde(default)]
    pub punctuated_word: Option<String>,
    #[serde(default)]
    pub start: f64,
    #[serde(default)]
    pub end: f64,
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub speaker: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct Alternative {
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    words: Vec<DeepgramWord>,
}

// Deepgram splits utterances by speaker when diarization is on, so each utterance is
// already a single-speaker turn. Prerecorded (file) transcription does not carry
// the numeric speaker id: there, diarization only shows as speaker-split
// segmentation. Realtime does carry it, as the number in "Speaker-Out 2" (see
// `segment_label`).
#[derive(Debug, Clone, Deserialize)]
pub struct Utterance {
    #[serde(default)]
    pub start: f64,
    #[serde(default)]
    pub end: f64,
    #[serde(default)]
    pub transcript: String,
    /// Diarization speaker id (0-based), present only when diarization is on.
    #[serde(default)]
    pub speaker: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Channel {
    #[serde(default)]
    alternatives: Vec<Alternative>,
}

#[derive(Debug, Deserialize)]
struct PrerecordedResults {
    #[serde(default)]
    channels: Vec<Channel>,
    #[serde(default)]
    utterances: Vec<Utterance>,
}

#[derive(Debug, Deserialize)]
struct PrerecordedMetadata {
    #[serde(default)]
    duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct PrerecordedResponse {
    results: PrerecordedResults,
    #[serde(default)]
    metadata: Option<PrerecordedMetadata>,
}

/// Result of a prerecorded transcription, normalized for segment mapping.
#[derive(Debug, Clone)]
pub struct DeepgramTranscript {
    pub text: String,
    pub utterances: Vec<Utterance>,
    pub words: Vec<DeepgramWord>,
    pub duration_seconds: Option<f64>,
}

impl From<PrerecordedResponse> for DeepgramTranscript {
    fn from(payload: PrerecordedResponse) -> Self {
        let (text, words) = payload
            .results
            .channels
            .into_iter()
            .next()
            .and_then(|channel| channel.alternatives.into_iter().next())
            .map(|alt| (alt.transcript, alt.words))
            .unwrap_or_default();
        DeepgramTranscript {
            text,
            utterances: payload.results.utterances,
            words,
            duration_seconds: payload.metadata.and_then(|m| m.duration),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RealtimeChannel {
    #[serde(default)]
    alternatives: Vec<Alternative>,
}

#[derive(Debug, Deserialize)]
struct RealtimeMessage {
    #[serde(default, rename = "type")]
    msg_type: Option<String>,
    #[serde(default)]
    channel: Option<RealtimeChannel>,
    #[serde(default)]
    is_final: bool,
    /// `[channel, total_channels]` (`[0, 1]` on mono). Kept as raw JSON and
    /// read by `parse_channel_index`, so an unexpected shape cannot reject the
    /// whole message.
    #[serde(default)]
    channel_index: serde_json::Value,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    duration: f64,
    // A `type: "Error"` message carries these; both are optional in the schema.
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Flux (`/v2/listen`) turn-based message. The finalized turn transcript arrives
/// on `event == "EndOfTurn"` (natively punctuated); Update/StartOfTurn/Eager/
/// TurnResumed are interim. `audio_window_start/end` are stream-relative seconds.
#[derive(Debug, Deserialize)]
struct FluxTurnInfo {
    #[serde(default, rename = "type")]
    msg_type: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    transcript: String,
    #[serde(default)]
    audio_window_start: f64,
    #[serde(default)]
    audio_window_end: f64,
    #[serde(default)]
    words: Vec<DeepgramWord>,
    // Present on a `type: "Error"` control message.
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// A finalized segment reduced from either protocol, in stream-relative ms.
struct FinalSeg {
    transcript: String,
    start_ms: f64,
    end_ms: f64,
    confidence: f32,
    /// Diarized speaker runs within this final; empty unless diarization is on.
    /// The final stays whole until emit so bleed suppression compares complete
    /// Mic-In and Speaker-Out text (split pieces fall below its overlap rules).
    turns: Vec<SpeakerTurn>,
    /// Word timings, so bleed matching can align a short mic line with the
    /// far side's words at that moment.
    words: Vec<TimedWord>,
}

/// One lowercase word token and when it was spoken (ms).
#[derive(Debug, Clone, PartialEq)]
struct TimedWord {
    token: String,
    start_ms: f64,
    end_ms: f64,
    /// Index of the Deepgram word this token came from (a word like "SOC-2"
    /// gives several tokens), and that word as displayed. Bleed trimming cuts
    /// a line at word boundaries and rebuilds its text from these.
    src: usize,
    word: String,
    /// The source word's confidence, when Deepgram gave one.
    confidence: Option<f32>,
}

fn timed_words(words: &[DeepgramWord]) -> Vec<TimedWord> {
    words
        .iter()
        .enumerate()
        .flat_map(|(src, w)| {
            let word = word_display(w).to_string();
            word_tokens(&w.word).into_iter().map(move |token| TimedWord {
                token,
                start_ms: w.start * 1000.0,
                end_ms: (w.end * 1000.0).max(w.start * 1000.0),
                src,
                word: word.clone(),
                confidence: w.confidence,
            })
        })
        .collect()
}

/// One run of same-speaker words in a diarized final.
#[derive(Debug, Clone, PartialEq)]
struct SpeakerTurn {
    text: String,
    start_ms: f64,
    end_ms: f64,
    confidence: f32,
    /// Deepgram's 0-based diarization speaker id.
    speaker: i64,
    /// The run's first and last word, as indexes into the final's word list
    /// (the `src` of its `TimedWord`s). This, not time, is what puts a word
    /// in a run when a line is cut: two speakers' words can share a start
    /// time, and adjacent runs touch.
    first_src: usize,
    last_src: usize,
}

/// A final nova `Results` message: which channel it covers and its segment,
/// if it had words.
struct NovaFinal {
    channel_index: Option<(usize, usize)>,
    seg: Option<FinalSeg>,
}

/// Reduce a nova message to a final (None for interim and non-Results).
fn nova_final(m: RealtimeMessage) -> Option<NovaFinal> {
    if m.msg_type.as_deref() != Some("Results") || !m.is_final {
        return None;
    }
    let channel_index = parse_channel_index(&m.channel_index);
    let start_ms = m.start * 1000.0;
    let end_ms = (m.start + m.duration) * 1000.0;
    let seg = m
        .channel
        .and_then(|c| c.alternatives.into_iter().next())
        .and_then(|alternative| {
            let transcript = alternative.transcript.trim();
            if transcript.is_empty() {
                return None;
            }
            let confidence = alternative.confidence.unwrap_or_else(|| {
                average_confidence(alternative.words.iter().filter_map(|w| w.confidence))
            });
            Some(FinalSeg {
                transcript: transcript.to_string(),
                start_ms,
                end_ms,
                confidence,
                turns: diarized_turns(&alternative.words),
                words: timed_words(&alternative.words),
            })
        });
    Some(NovaFinal { channel_index, seg })
}

/// A word's display text: the punctuated form unless it is missing or empty.
fn word_display(w: &DeepgramWord) -> &str {
    w.punctuated_word.as_deref().filter(|p| !p.is_empty()).unwrap_or(&w.word)
}

/// Runs of same-speaker words, in stream ms. Empty when no word carries a
/// speaker id (diarization off). A word missing its id stays with the current
/// run rather than splitting a line into an unnumbered fragment.
fn diarized_turns(words: &[DeepgramWord]) -> Vec<SpeakerTurn> {
    let mut turns: Vec<SpeakerTurn> = Vec::new();
    let mut confidences: Vec<Vec<f32>> = Vec::new();
    let Some(mut speaker) = words.iter().find_map(|w| w.speaker) else {
        return turns;
    };
    for (i, w) in words.iter().enumerate() {
        let display = word_display(w);
        if display.is_empty() {
            continue;
        }
        speaker = w.speaker.unwrap_or(speaker);
        match turns.last_mut() {
            Some(last) if last.speaker == speaker => {
                last.text.push(' ');
                last.text.push_str(display);
                last.end_ms = (w.end * 1000.0).max(last.end_ms);
                last.last_src = i;
            }
            _ => {
                turns.push(SpeakerTurn {
                    text: display.to_string(),
                    start_ms: w.start * 1000.0,
                    end_ms: (w.end * 1000.0).max(w.start * 1000.0),
                    confidence: 0.0,
                    speaker,
                    first_src: i,
                    last_src: i,
                });
                confidences.push(Vec::new());
            }
        }
        if let (Some(c), Some(list)) = (w.confidence, confidences.last_mut()) {
            list.push(c);
        }
    }
    for (turn, list) in turns.iter_mut().zip(confidences) {
        turn.confidence = average_confidence(list.into_iter());
    }
    turns
}

/// Reduce a nova `Results` message to a finalized segment (None for interim/other).
/// Test shorthand: the realtime reader uses `nova_final` for the channel too.
#[cfg(test)]
fn nova_result_to_final(m: RealtimeMessage) -> Option<FinalSeg> {
    nova_final(m)?.seg
}

/// Reduce a Flux `TurnInfo` to a finalized segment (only on `EndOfTurn`).
fn flux_turn_to_final(m: &FluxTurnInfo) -> Option<FinalSeg> {
    if m.event.as_deref() != Some("EndOfTurn") {
        return None;
    }
    let transcript = m.transcript.trim();
    if transcript.is_empty() {
        return None;
    }
    // The turn's time is its words' span. DO NOT use audio_window_start/end
    // when words are present: Flux's window runs from the end of the previous
    // turn, silence included, so the first turn of a call "started" at 0:00
    // and sorted above earlier Mic-In speech (measured 2026-11-18, WORK-559).
    let (start_ms, end_ms) = match (m.words.first(), m.words.last()) {
        (Some(first), Some(last)) => (first.start * 1000.0, (last.end * 1000.0).max(first.start * 1000.0)),
        _ => (m.audio_window_start * 1000.0, m.audio_window_end * 1000.0),
    };
    Some(FinalSeg {
        transcript: transcript.to_string(),
        start_ms,
        end_ms,
        confidence: average_confidence(m.words.iter().filter_map(|w| w.confidence)),
        // Flux has no diarization.
        turns: Vec::new(),
        words: timed_words(&m.words),
    })
}

/// The in-progress text of a Flux turn: every TurnInfo except EndOfTurn
/// (Update, StartOfTurn, EagerEndOfTurn, TurnResumed) carries the turn so
/// far. None when it has no words yet.
fn flux_turn_to_interim(m: &FluxTurnInfo) -> Option<String> {
    if m.event.as_deref() == Some("EndOfTurn") {
        return None;
    }
    let transcript = m.transcript.trim();
    (!transcript.is_empty()).then(|| transcript.to_string())
}

/// A nova interim `Results` message (`is_final: false`): the channel it
/// covers, its text and its words. Empty text is a revision to nothing and
/// clears the channel's greyed line. None for finals and other messages.
fn nova_interim(m: &RealtimeMessage) -> Option<(Option<(usize, usize)>, String, Vec<DeepgramWord>)> {
    if m.msg_type.as_deref() != Some("Results") || m.is_final {
        return None;
    }
    let alt = m.channel.as_ref()?.alternatives.first()?;
    Some((parse_channel_index(&m.channel_index), alt.transcript.trim().to_string(), alt.words.clone()))
}

/// The interim words a nova final did not cover. A final can end mid-way
/// through the last interim (an interim over 0-4.3 s, then a final over
/// 0-3.3 s), and the rest is still being heard: keep it greyed instead of
/// blanking it until the next interim. Times are stream seconds.
///
/// Limit: a word is kept when its midpoint is at or after the final's end,
/// and dropped otherwise, judged on the interim's timing alone.
fn interim_after(words: &[DeepgramWord], final_end_s: f64) -> Vec<DeepgramWord> {
    // By midpoint: interim and final word boundaries can differ by a few ms,
    // and a word the final already holds ends by its end.
    words.iter().filter(|w| (w.start + w.end) / 2.0 >= final_end_s).cloned().collect()
}

fn words_line(words: &[DeepgramWord]) -> String {
    words.iter().map(word_display).collect::<Vec<_>>().join(" ")
}

/// Maps realtime-stream time onto original recording time. The pipeline feeds a
/// continuous mixed stream while carrying each chunk's recording-relative
/// timestamp, so realtime word times (stream-relative) are remapped back.
#[derive(Debug, Clone)]
pub struct AudioTimeMapEntry {
    pub stream_start_ms: f64,
    pub stream_end_ms: f64,
    pub original_start_ms: f64,
    pub original_end_ms: f64,
}

// ============================================================================
// CLIENT (prerecorded REST)
// ============================================================================

#[derive(Debug, Clone)]
pub struct DeepgramClient {
    api_key: String,
    http: reqwest::Client,
    options: DeepgramOptions,
}

impl DeepgramClient {
    pub fn new(api_key: String, options: DeepgramOptions) -> Self {
        Self {
            api_key,
            http: reqwest::Client::new(),
            options,
        }
    }

    /// Transcribe a local file via the prerecorded REST API. This is a single
    /// synchronous request (no upload -> create -> poll -> fetch cycle),
    /// so progress is coarse: submit, then parse.
    pub async fn transcribe_file_with_progress<F, C>(
        &self,
        path: &Path,
        language: Option<String>,
        _client_reference_id: &str,
        mut progress: F,
        mut should_cancel: C,
    ) -> Result<DeepgramTranscript>
    where
        F: FnMut(u32, &str),
        C: FnMut() -> bool,
    {
        ensure_not_cancelled(&mut should_cancel)?;
        progress(10, "Reading audio file...");
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read audio file: {}", path.display()))?;

        ensure_not_cancelled(&mut should_cancel)?;
        progress(25, "Uploading audio to Deepgram...");

        let query = self.prerecorded_query(language);
        let content_type = mime_for_path(path);
        let response = self
            .http
            .post(LISTEN_REST_URL)
            .query(&query)
            .header("Authorization", format!("Token {}", self.api_key))
            .header("Content-Type", content_type)
            .body(bytes)
            .send()
            .await
            .context("Failed to submit audio to Deepgram")?;

        let response = ensure_success(response, "prerecorded transcription").await?;

        ensure_not_cancelled(&mut should_cancel)?;
        progress(85, "Parsing Deepgram transcript...");
        let payload: PrerecordedResponse = response
            .json()
            .await
            .context("Failed to parse Deepgram prerecorded response")?;

        Ok(DeepgramTranscript::from(payload))
    }

    /// Query parameters for the prerecorded endpoint. `smart_format` and
    /// `punctuate` give readable transcripts; `utterances` yields speaker turns.
    fn prerecorded_query(&self, language: Option<String>) -> Vec<(String, String)> {
        let model = self.prerecorded_model();
        let mut query = vec![
            ("model".to_string(), model.to_string()),
            ("smart_format".to_string(), "true".to_string()),
            ("punctuate".to_string(), "true".to_string()),
            ("utterances".to_string(), "true".to_string()),
            ("paragraphs".to_string(), "true".to_string()),
        ];
        if self.options.diarize {
            // Deepgram's current diarizer. `diarize_model` is mutually exclusive with
            // `diarize`/`diarize_version` (sending both -> HTTP 400), so emit ONLY
            // diarize_model=latest here, never diarize=true. Verified live against
            // api.deepgram.com (both prerecorded and streaming return 200/101).
            query.push(("diarize_model".to_string(), "latest".to_string()));
        }
        if self.options.mip_opt_out {
            query.push(("mip_opt_out".to_string(), "true".to_string()));
        }
        // Deepgram-specific language wins; fall back to the caller's hint if unset.
        let lang = self.options.language.as_deref().or(language.as_deref());
        apply_language(&mut query, lang, model);
        apply_keyterms(&mut query, &self.options.keyterms);
        query
    }

    /// The model to use for prerecorded (file) transcription. Flux is a
    /// realtime-only API with no `/v1/listen` prerecorded support, so imported
    /// audio / retranscription falls back to the default nova async model.
    fn prerecorded_model(&self) -> &str {
        if is_flux_model(&self.options.model) {
            crate::config::DEFAULT_DEEPGRAM_ASYNC_MODEL
        } else {
            &self.options.model
        }
    }
}

// ============================================================================
// CONFIG READERS
// ============================================================================

pub async fn configured_api_key<R: Runtime>(app: &AppHandle<R>) -> Result<String> {
    let app_state = app
        .try_state::<AppState>()
        .ok_or_else(|| anyhow!("App state not available"))?;

    let stored_key =
        SettingsRepository::get_transcript_api_key(app_state.db_manager.pool(), PROVIDER)
            .await
            .context("Failed to read Deepgram API key from settings")?;

    let key = stored_key
        .filter(|key| !key.trim().is_empty())
        .or_else(|| std::env::var("DEEPGRAM_API_KEY").ok())
        .unwrap_or_default();

    if key.trim().is_empty() {
        return Err(anyhow!(
            "Deepgram API key is not configured. Add it in Transcription settings or set DEEPGRAM_API_KEY."
        ));
    }

    Ok(key)
}

pub async fn validate_configured_api_key<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    configured_api_key(app)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Reads the persisted Deepgram options: model (from transcript config) plus
/// keyterms/diarize (from the dedicated Deepgram settings columns).
pub async fn configured_options<R: Runtime>(app: &AppHandle<R>) -> Result<DeepgramOptions> {
    let app_state = app
        .try_state::<AppState>()
        .ok_or_else(|| anyhow!("App state not available"))?;
    let pool = app_state.db_manager.pool();

    let mut options = DeepgramOptions::default();

    if let Ok(Some(config)) = SettingsRepository::get_transcript_config(pool).await {
        if config.provider == PROVIDER && !config.model.trim().is_empty() {
            options.model = config.model;
        }
    }

    let row = SettingsRepository::get_deepgram_options(pool)
        .await
        .unwrap_or_default();
    options.keyterms = parse_keyterms(row.keyterm.as_deref());
    if let Some(diarize) = row.diarize {
        options.diarize = diarize != 0;
    }
    if let Some(mip_opt_out) = row.mip_opt_out {
        options.mip_opt_out = mip_opt_out != 0;
    }
    options.language = row.language.filter(|l| !l.trim().is_empty());

    Ok(options)
}

pub async fn client_from_config<R: Runtime>(app: &AppHandle<R>) -> Result<DeepgramClient> {
    let api_key = configured_api_key(app).await?;
    let options = configured_options(app).await?;
    Ok(DeepgramClient::new(api_key, options))
}

// ============================================================================
// SEGMENT MAPPING (prerecorded)
// ============================================================================

/// Convert a prerecorded transcript to segments. Prefers `utterances` (already
/// speaker-segmented turns); falls back to word grouping, then to a single
/// segment covering the whole file.
pub fn transcript_to_segments(
    transcript: &DeepgramTranscript,
    fallback_duration_seconds: f64,
) -> Vec<TranscriptSegment> {
    if !transcript.utterances.is_empty() {
        let segments = labeled_segments(utterances_to_tuples(&transcript.utterances));
        if !segments.is_empty() {
            return segments;
        }
    }

    let mut segments = labeled_segments(words_to_tuples(&transcript.words));
    if segments.is_empty() && !transcript.text.trim().is_empty() {
        // Prefer Deepgram's own reported duration; fall back to the caller's.
        let total_duration = transcript.duration_seconds.unwrap_or(fallback_duration_seconds);
        segments.push(TranscriptSegment {
            id: format!("transcript-{}", uuid::Uuid::new_v4()),
            text: transcript.text.trim().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            audio_start_time: Some(0.0),
            audio_end_time: Some(total_duration),
            duration: Some(total_duration),
            // Prerecorded files are transcribed as one mixed signal.
            speaker: None,
        });
    }
    segments
}

/// (text, start_ms, end_ms, diarized speaker id) for one prerecorded segment.
type SpeakerTuple = (String, f64, f64, Option<i64>);

/// Segments for prerecorded output. The recording file is one mixed signal,
/// so there is no Mic-In / Speaker-Out channel; a diarized speaker is still
/// kept, as "Speaker N" (the same `segment_label` realtime uses).
fn labeled_segments(tuples: Vec<SpeakerTuple>) -> Vec<TranscriptSegment> {
    let (plain, speakers): (Vec<(String, f64, f64)>, Vec<Option<i64>>) =
        tuples.into_iter().map(|(t, s, e, spk)| ((t, s, e), spk)).unzip();
    // create_transcript_segments maps its input 1:1, so the zip lines up.
    super::common::create_transcript_segments(&plain)
        .into_iter()
        .zip(speakers)
        .map(|(mut seg, spk)| {
            seg.speaker = segment_label(None, spk);
            seg
        })
        .collect()
}

fn utterances_to_tuples(utterances: &[Utterance]) -> Vec<SpeakerTuple> {
    let mut tuples = Vec::new();
    for utterance in utterances {
        push_tuple(
            &mut tuples,
            utterance.transcript.clone(),
            utterance.start * 1000.0,
            utterance.end * 1000.0,
            utterance.speaker,
        );
    }
    tuples
}

/// Group word-level output into segments on speaker change, long pause, or a
/// long sentence-terminated run, so transcript rows stay a readable size.
fn words_to_tuples(words: &[DeepgramWord]) -> Vec<SpeakerTuple> {
    let mut segments = Vec::new();
    let mut current_text = String::new();
    let mut current_start_ms = 0.0;
    let mut current_end_ms = 0.0;
    let mut current_speaker: Option<i64> = None;

    for word in words {
        let display = word_display(word);
        if display.is_empty() {
            continue;
        }

        let word_start_ms = word.start * 1000.0;
        let word_end_ms = (word.end * 1000.0).max(word_start_ms);
        let speaker_changed = current_speaker.is_some() && word.speaker != current_speaker;
        let long_gap = !current_text.is_empty() && word_start_ms - current_end_ms > 1500.0;
        let long_segment =
            word_start_ms - current_start_ms > 30_000.0 && ends_sentence(&current_text);

        if !current_text.is_empty() && (speaker_changed || long_gap || long_segment) {
            push_tuple(
                &mut segments,
                std::mem::take(&mut current_text),
                current_start_ms,
                current_end_ms,
                current_speaker,
            );
            current_speaker = None;
        }

        if current_text.is_empty() {
            current_start_ms = word_start_ms;
            current_speaker = word.speaker;
        } else {
            current_text.push(' ');
        }

        current_text.push_str(display);
        current_end_ms = word_end_ms;
    }

    push_tuple(&mut segments, current_text, current_start_ms, current_end_ms, current_speaker);
    segments
}

// ============================================================================
// SOURCE LABELS
// ============================================================================

/// Transcript label for the microphone channel.
/// DO NOT change without frontend/src/lib/transcript-speakers.ts
/// (`ts_speaker_labels_match_rust_constants` enforces they match).
pub const MIC_IN_LABEL: &str = "Mic-In";
/// Transcript label for the system-output channel. Same constraint as above.
pub const SPEAKER_OUT_LABEL: &str = "Speaker-Out";

/// The single mapping from a realtime channel to its transcript label.
/// DO NOT reorder: the pipeline interleaves stereo frames as [mic, system]
/// (pipeline.rs `ContinuousStereo`), so channel 0 is the mic. A `Mono` stream
/// has no single source and gets no label.
pub fn channel_label(layout: AudioLayout, channel: usize) -> Option<&'static str> {
    match (layout, channel) {
        (AudioLayout::MicOnly, 0) | (AudioLayout::MicSystemStereo, 0) => Some(MIC_IN_LABEL),
        (AudioLayout::MicSystemStereo, 1) => Some(SPEAKER_OUT_LABEL),
        _ => None,
    }
}

/// One realtime WebSocket and the input channels it carries. nova takes every
/// channel on a single socket (`channels=N&multichannel=true`). Flux has no
/// multichannel support (`/v2/listen` answers HTTP 400 "Unknown query
/// parameters: channels, multichannel", verified live), so it gets one mono
/// socket per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lane {
    first_channel: usize,
    channels: usize,
}

fn plan_lanes(flux: bool, input_channels: usize) -> Vec<Lane> {
    let input_channels = input_channels.clamp(1, 2);
    if flux {
        (0..input_channels)
            .map(|first_channel| Lane { first_channel, channels: 1 })
            .collect()
    } else {
        vec![Lane { first_channel: 0, channels: input_channels }]
    }
}

/// Global input channel of a result, from the lane and the `channel_index`
/// Deepgram reported. A mono socket is unambiguous. On a multichannel socket a
/// missing or out-of-range index yields None: the line is emitted unlabelled
/// rather than guessed (a guess of 0 would call the far side "Mic-In").
fn resolve_channel(lane: Lane, reported: Option<(usize, usize)>) -> Option<usize> {
    if lane.channels == 1 {
        return Some(lane.first_channel);
    }
    match reported {
        Some((channel, total)) if channel < lane.channels && total == lane.channels => {
            Some(lane.first_channel + channel)
        }
        _ => None,
    }
}

/// Lenient read of `channel_index` (`[channel, total]`). Any other shape is
/// None instead of failing the whole Results message, which would drop its
/// transcript.
fn parse_channel_index(value: &serde_json::Value) -> Option<(usize, usize)> {
    match value.as_array()?.as_slice() {
        [channel, total] => Some((channel.as_u64()? as usize, total.as_u64()? as usize)),
        _ => None,
    }
}

/// Resampled channels may differ by a sample or two (rounding); more than this
/// means the conversion went wrong and the chunk is dropped.
const MAX_CHANNEL_LENGTH_SKEW: usize = 2;

/// Convert one pipeline chunk into a lane's 16 kHz samples, interleaved when
/// the lane carries two channels. Each channel is resampled on its own:
/// resampling interleaved data would smear the two sources into each other.
/// None means the chunk must be dropped (logged): a channel failed to
/// resample, or the channels came out at different lengths. Sending audio at
/// the wrong rate, or interleaving mismatched rates, would garble the
/// transcript and skew every later timestamp.
fn lane_samples(chunk: &AudioChunk, lane: Lane) -> Option<Vec<f32>> {
    lane_samples_with(chunk, lane, |samples, from, to| {
        super::audio_processing::resample(samples, from, to)
    })
}

fn lane_samples_with<F>(chunk: &AudioChunk, lane: Lane, resample: F) -> Option<Vec<f32>>
where
    F: Fn(&[f32], u32, u32) -> Result<Vec<f32>>,
{
    let mut picked = Vec::with_capacity(lane.channels);
    for (offset, samples) in deinterleave(&chunk.data, chunk.layout.channels())
        .into_iter()
        .skip(lane.first_channel)
        .take(lane.channels)
        .enumerate()
    {
        if chunk.sample_rate == PCM_SAMPLE_RATE {
            picked.push(samples);
            continue;
        }
        match resample(&samples, chunk.sample_rate, PCM_SAMPLE_RATE) {
            Ok(resampled) => picked.push(resampled),
            Err(e) => {
                error!(
                    "Deepgram realtime: dropping chunk {}: channel {} failed to resample {} -> {} Hz: {}",
                    chunk.chunk_id,
                    lane.first_channel + offset,
                    chunk.sample_rate,
                    PCM_SAMPLE_RATE,
                    e
                );
                return None;
            }
        }
    }
    match picked.len() {
        0 => Some(Vec::new()),
        1 => picked.pop(),
        _ => {
            let (left, right) = (&picked[0], &picked[1]);
            let skew = left.len().abs_diff(right.len());
            if skew > MAX_CHANNEL_LENGTH_SKEW {
                warn!(
                    "Deepgram realtime: dropping chunk {}: channels resampled to {} vs {} samples",
                    chunk.chunk_id,
                    left.len(),
                    right.len()
                );
                return None;
            }
            Some(interleave_stereo(left, right))
        }
    }
}

/// Recording-relative duration of a chunk in ms, counted in frames so an
/// interleaved stereo chunk is not reported as twice its real length.
fn chunk_duration_ms(chunk: &AudioChunk) -> f64 {
    let frames = chunk.data.len() / chunk.layout.channels() as usize;
    frames as f64 / chunk.sample_rate as f64 * 1000.0
}

// ============================================================================
// BLEED SUPPRESSION
// ============================================================================

/// Recording-time slack when deciding two segments are the same utterance.
const BLEED_MAX_GAP_MS: f64 = 2000.0;
/// Token Jaccard at or above which two segments are the same words.
const BLEED_MIN_JACCARD: f64 = 0.8;
/// For a partial copy (the channels segment differently), the share of the
/// shorter segment's tokens found in the longer one, if it has enough tokens
/// that a coincidental overlap is implausible.
const BLEED_MIN_CONTAINMENT: f64 = 0.9;
const BLEED_MIN_CONTAINMENT_TOKENS: usize = 4;
/// Segments shorter than this ("Okay.", "Yeah, sure.") match genuine replies
/// too easily, so they also have to coincide in time: acoustic bleed is
/// near-instant, a reply comes after the far side. With word timings each
/// word must start within BLEED_SHORT_MAX_START_DELTA_MS of the same far-side
/// word (`is_bleed_of`); without, the segments must (`is_bleed`).
const BLEED_SHORT_TOKENS: usize = 3;
const BLEED_SHORT_MAX_START_DELTA_MS: f64 = 300.0;
const BLEED_SHORT_MIN_OVERLAP: f64 = 0.5;
/// How long each channel's finals are remembered for matching: Speaker-Out
/// finals against late Mic-In copies, shown Mic-In lines against late
/// Speaker-Out finals (which then retract them).
const BLEED_MEMORY_MS: f64 = 30_000.0;

/// A finalized realtime segment in recording-relative ms, ready to emit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LabeledSegment {
    text: String,
    start_ms: f64,
    end_ms: f64,
    confidence: f32,
    speaker: Option<&'static str>,
    /// Diarized runs (recording-relative ms); empty unless diarization is on.
    /// Bleed suppression ignores these and compares the whole segment.
    turns: Vec<SpeakerTurn>,
    /// Word timings in recording ms (empty if Deepgram sent none).
    words: Vec<TimedWord>,
}

fn word_tokens(text: &str) -> std::collections::HashSet<String> {
    word_list(text).into_iter().collect()
}

/// The lowercase words of `text`, in order.
fn word_list(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect()
}

/// True when `mic` is the far side's audio leaking into the microphone
/// (speakers into the mic, or an echo path) rather than the user speaking.
/// Measured on a speakers-on Slack huddle: 82 of 109 mic segments were
/// verbatim copies of a concurrent system-audio segment.
fn is_bleed(mic: &LabeledSegment, speaker: &LabeledSegment) -> bool {
    let close_in_time = mic.start_ms <= speaker.end_ms + BLEED_MAX_GAP_MS
        && speaker.start_ms <= mic.end_ms + BLEED_MAX_GAP_MS;
    if !close_in_time {
        return false;
    }
    let a = word_tokens(&mic.text);
    let b = word_tokens(&speaker.text);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let shared = a.intersection(&b).count() as f64;
    let jaccard = shared / a.union(&b).count() as f64;
    let smaller = a.len().min(b.len());
    let same_words = jaccard >= BLEED_MIN_JACCARD
        || (smaller >= BLEED_MIN_CONTAINMENT_TOKENS && shared / smaller as f64 >= BLEED_MIN_CONTAINMENT);
    if !same_words {
        return false;
    }
    if smaller < BLEED_SHORT_TOKENS {
        let start_delta = (mic.start_ms - speaker.start_ms).abs();
        let overlap = (mic.end_ms.min(speaker.end_ms) - mic.start_ms.max(speaker.start_ms)).max(0.0);
        let mic_duration = (mic.end_ms - mic.start_ms).max(1.0);
        return start_delta <= BLEED_SHORT_MAX_START_DELTA_MS
            && overlap / mic_duration >= BLEED_SHORT_MIN_OVERLAP;
    }
    true
}

/// How far around a mic line to gather the far side's words: the channels
/// segment speech differently, so one mic line can span several Speaker-Out
/// finals and start or end slightly off theirs.
const BLEED_WORD_WINDOW_MS: f64 = 1500.0;
/// Share of the mic line's words that must appear, in order, among the far
/// side's words in that window.
const BLEED_MIN_WORD_MATCH: f64 = 0.8;

/// Length of the longest common subsequence of two token lists.
fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    let mut prev = vec![0usize; b.len() + 1];
    for x in a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = if x == y { prev[j] + 1 } else { prev[j + 1].max(cur[j]) };
        }
        prev = cur;
    }
    prev[b.len()]
}

/// True when `mic` repeats what the far side said at the same moment, judged
/// against all of `speakers` together, using Deepgram's word timings (the
/// span of the words, NOT the final's window, which includes silence):
/// - a line of BLEED_SHORT_TOKENS or more is echo when most of its words
///   appear, in order, among the far side's words around it, however each
///   channel split the speech into finals;
/// - a shorter line ("Sounds great.") is echo only when each of its words
///   matches the same far-side word starting within
///   BLEED_SHORT_MAX_START_DELTA_MS, so a genuine "Yeah." over a far-side
///   monologue that says "yeah" elsewhere is kept.
/// Without word timings, falls back to comparing whole segments (`is_bleed`).
fn is_bleed_of(mic: &LabeledSegment, speakers: &[&LabeledSegment]) -> bool {
    let (Some(first), Some(last)) = (mic.words.first(), mic.words.last()) else {
        return speakers.iter().any(|s| is_bleed(mic, s));
    };
    if speakers.iter().all(|s| s.words.is_empty()) {
        return speakers.iter().any(|s| is_bleed(mic, s));
    }
    let far_words = speakers.iter().flat_map(|s| s.words.iter());
    if mic.words.len() < BLEED_SHORT_TOKENS {
        let far: Vec<&TimedWord> = far_words.collect();
        return mic.words.iter().all(|m| {
            far.iter().any(|f| {
                f.token == m.token && (f.start_ms - m.start_ms).abs() <= BLEED_SHORT_MAX_START_DELTA_MS
            })
        });
    }
    let (from, to) = (first.start_ms - BLEED_WORD_WINDOW_MS, last.end_ms + BLEED_WORD_WINDOW_MS);
    let mut far: Vec<&TimedWord> = far_words.filter(|w| w.start_ms <= to && w.end_ms >= from).collect();
    far.sort_by(|a, b| a.start_ms.total_cmp(&b.start_ms));
    let mic_tokens: Vec<&str> = mic.words.iter().map(|w| w.token.as_str()).collect();
    let far_tokens: Vec<&str> = far.iter().map(|w| w.token.as_str()).collect();
    lcs_len(&mic_tokens, &far_tokens) as f64 / mic_tokens.len() as f64 >= BLEED_MIN_WORD_MATCH
}

/// Fewest tokens a trimmed-off bleed run may have: shorter shared runs
/// ("yeah, okay") are too likely to be the user's own words.
const BLEED_TRIM_MIN_TOKENS: usize = BLEED_MIN_CONTAINMENT_TOKENS;

/// The part of `mic` to keep when only its start or its end is the far
/// side's audio, or None when there is nothing to trim.
///
/// Flux ends a turn on its own turn model, not on 300 ms of silence like
/// nova's endpointing, so on the Pipecat engine the user's words and the
/// bleed of the far side's reply that follows them come back as ONE Mic-In
/// line (measured 2026-11-18, WORK-559: "Thanks for joining today. ... Yes.
/// We handle about forty thousand calls a day, ..." where everything from
/// "Yes." on was bleed). Whole-line matching cannot remove that, so the line
/// is cut at a word boundary: the longest run at its end (else at its start)
/// that is bleed by the same timed-word rule as `is_bleed_of`, with at least
/// BLEED_TRIM_MIN_TOKENS tokens, leaving a part that is NOT bleed.
/// Needs word timings; diarized lines are left whole.
fn trim_bleed(mic: &LabeledSegment, speakers: &[&LabeledSegment]) -> Option<LabeledSegment> {
    if mic.words.is_empty() || !mic.turns.is_empty() || speakers.iter().all(|s| s.words.is_empty()) {
        return None;
    }
    let first_src = mic.words.first()?.src;
    let last_src = mic.words.last()?.src;
    let part = |from: usize, to: usize| with_words(mic, |w| w.src >= from && w.src <= to);
    let is_trimmable_bleed = |seg: &LabeledSegment| seg.words.len() >= BLEED_TRIM_MIN_TOKENS && is_bleed_of(seg, speakers);
    // A source word the far side said at the same moment. The run test above
    // tolerates 20% misses, so without this edge check the cut would swallow
    // the user's last (or first) few words into the bleed.
    let far: Vec<&TimedWord> = speakers.iter().flat_map(|s| s.words.iter()).collect();
    let echoed = |src: usize| mic.words.iter().filter(|w| w.src == src).any(|m| is_echoed(m, &far));
    let accept = |keep: Option<LabeledSegment>, bleed: Option<LabeledSegment>| match (keep, bleed) {
        (Some(keep), Some(bleed)) if is_trimmable_bleed(&bleed) && !is_bleed_of(&keep, speakers) => Some(keep),
        _ => None,
    };
    // Bleed at the end: the user spoke, then the far side's reply leaked in.
    if let Some(first_cut) =
        ((first_src + 1)..=last_src).find(|&cut| accept(part(first_src, cut - 1), part(cut, last_src)).is_some())
    {
        // Move the cut to the first echoed word, if that still passes.
        let cut = (first_cut..=last_src).find(|&c| echoed(c)).unwrap_or(first_cut);
        if let Some(keep) = accept(part(first_src, cut - 1), part(cut, last_src)) {
            return Some(keep);
        }
        return accept(part(first_src, first_cut - 1), part(first_cut, last_src));
    }
    // Bleed at the start: the far side was still finishing when the user began.
    if let Some(first_cut) =
        ((first_src + 1)..=last_src).rev().find(|&cut| accept(part(cut, last_src), part(first_src, cut - 1)).is_some())
    {
        // Move the cut back to just after the last echoed word.
        let cut = ((first_src + 1)..=first_cut).rev().find(|&c| echoed(c - 1)).unwrap_or(first_cut);
        if let Some(keep) = accept(part(cut, last_src), part(first_src, cut - 1)) {
            return Some(keep);
        }
        return accept(part(first_cut, last_src), part(first_src, first_cut - 1));
    }
    None
}

/// `seg` cut down to the words `keep` selects, its text rebuilt from those
/// words as displayed, its time from their span and its confidence from
/// theirs. None when no word is kept. Diarized runs are clipped the same way:
/// each kept word stays in the run it was in (by its `src`, the run's word
/// range; time cannot decide it: adjacent runs touch, and two speakers'
/// words can start together). Each run's text, start and confidence come
/// from its own kept words, and a run left with none is dropped.
fn with_words(seg: &LabeledSegment, keep: impl Fn(&TimedWord) -> bool) -> Option<LabeledSegment> {
    let words: Vec<TimedWord> = seg.words.iter().filter(|w| keep(w)).cloned().collect();
    let (first, last) = (words.first()?, words.last()?);
    let run_of = |w: &TimedWord| seg.turns.iter().position(|t| (t.first_src..=t.last_src).contains(&w.src));
    let turns = seg
        .turns
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let inside: Vec<&TimedWord> = words.iter().filter(|w| run_of(w) == Some(i)).collect();
            let first = inside.first()?;
            Some(SpeakerTurn {
                text: words_text(inside.iter().copied()),
                start_ms: first.start_ms,
                confidence: kept_confidence(inside.iter().copied(), t.confidence),
                ..t.clone()
            })
        })
        .collect();
    Some(LabeledSegment {
        text: words_text(words.iter()),
        start_ms: first.start_ms,
        end_ms: last.end_ms,
        confidence: kept_confidence(words.iter(), seg.confidence),
        speaker: seg.speaker,
        turns,
        words,
    })
}

/// Mean confidence of the source words behind `words` (each counted once),
/// or `fallback` when none carries one.
fn kept_confidence<'a>(words: impl Iterator<Item = &'a TimedWord>, fallback: f32) -> f32 {
    let mut prev = None;
    let (mut sum, mut n) = (0.0f32, 0u32);
    for w in words {
        if prev == Some(w.src) {
            continue;
        }
        prev = Some(w.src);
        if let Some(c) = w.confidence {
            sum += c;
            n += 1;
        }
    }
    if n == 0 {
        fallback
    } else {
        sum / n as f32
    }
}

/// The displayed words behind `words`, once per source word.
fn words_text<'a>(words: impl Iterator<Item = &'a TimedWord>) -> String {
    let mut text: Vec<&str> = Vec::new();
    let mut prev = None;
    for w in words {
        if prev != Some(w.src) {
            text.push(&w.word);
            prev = Some(w.src);
        }
    }
    text.join(" ")
}

/// A mic token the far side said around the same moment: same token, start
/// within BLEED_WORD_WINDOW_MS (the window `is_bleed_of` matches in, so a cut
/// is placed with the same timing tolerance the match was made with).
fn is_echoed(m: &TimedWord, far: &[&TimedWord]) -> bool {
    far.iter().any(|f| f.token == m.token && (f.start_ms - m.start_ms).abs() <= BLEED_WORD_WINDOW_MS)
}

/// At most half of `seg`'s source words echoed. Counted in source words, not
/// tokens ("SOC-2" is one word, two tokens), and a word is echoed when any of
/// its tokens is, the same rule `trim_bleed` places its cut with.
fn half_echoed_at_most(seg: &LabeledSegment, far_words: &[&TimedWord]) -> bool {
    let mut all = std::collections::BTreeSet::new();
    let mut echoed = std::collections::BTreeSet::new();
    for w in &seg.words {
        all.insert(w.src);
        if is_echoed(w, far_words) {
            echoed.insert(w.src);
        }
    }
    echoed.len() * 2 <= all.len()
}

/// What bleed suppression does with one Mic-In line.
#[derive(Debug, PartialEq)]
enum MicVerdict {
    /// Not bleed: show as is.
    Keep,
    /// Show only this part; the rest was the far side.
    Trim(LabeledSegment),
    /// All of it was the far side.
    Drop,
}

/// Judge a Mic-In line against the far side's recent finals. A line can pass
/// the whole-line rule (80% of its words echoed) and still begin or end with
/// the user's own words (two words of reply, then eight of bleed), so a trim
/// is tried first. On a line that is bleed as a whole, the trim is accepted
/// only if at most half of the kept words were said by the far side around
/// then: "yes absolutely" survives a nearby far-side "yes", while a misheard
/// echo ("we handle about 40 000" for "...forty thousand") is mostly echoed
/// words and is dropped with the rest.
fn judge_mic(mic: &LabeledSegment, far: &[&LabeledSegment]) -> MicVerdict {
    let trimmed = trim_bleed(mic, far);
    if is_bleed_of(mic, far) {
        let far_words: Vec<&TimedWord> = far.iter().flat_map(|s| s.words.iter()).collect();
        return match trimmed {
            Some(keep) if half_echoed_at_most(&keep, &far_words) => MicVerdict::Trim(keep),
            _ => MicVerdict::Drop,
        };
    }
    trimmed.map_or(MicVerdict::Keep, MicVerdict::Trim)
}

/// What the realtime session tells the transcript: show a line, or take back
/// a line it showed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RealtimeOutput {
    Show { key: u64, seg: LabeledSegment },
    /// A shown Mic-In line turned out to be Speaker-Out bleed.
    Retract { key: u64 },
    /// The not-yet-final words on one channel, shown greyed until its next
    /// final replaces them. Empty `text` clears the channel. Never stored:
    /// it goes out as `transcript-interim`, not `transcript-update`.
    Interim { speaker: Option<&'static str>, text: String },
}

/// Hides greyed Mic-In words that are Speaker-Out bleed (the far side heard
/// through the mic), which would otherwise flash on every far-side sentence
/// when the user is on speakers. Interims carry no reliable word timing to
/// match on, so this compares words against what Speaker-Out said in the last
/// INTERIM_BLEED_WINDOW_MS. Display only: a wrong guess just hides a grey
/// line until its final, which still goes through `BleedSuppressor`.
///
/// Limit: each Speaker-Out text (interim or final) stamps the current time on
/// every word after its common prefix with the previous Speaker-Out text,
/// compared as `word_list` words (lowercased, punctuation dropped), including
/// words already seen. Mic-In grey lines are judged against those
/// stamps for INTERIM_BLEED_WINDOW_MS.
#[derive(Default)]
struct InterimBleedFilter {
    /// Each far-side word and when it was last newly said.
    far: std::collections::HashMap<String, f64>,
    /// The far side's last update, as ordered words. Flux and nova resend the
    /// whole utterance on every update; only words past what this already
    /// held are new, so an utterance's first words do not stay "recent" for
    /// as long as the far side keeps talking.
    last_far: Vec<String>,
}

/// How far back Speaker-Out words count as the source of Mic-In bleed.
/// Short: bleed reaches the mic as the far side speaks, so its interims come
/// within a second of the far side's; a longer window would hide the user
/// repeating the far side's words back to them.
const INTERIM_BLEED_WINDOW_MS: f64 = 3_000.0;
/// Share of a Mic-In interim's words the far side said for it to be bleed.
const INTERIM_BLEED_MIN_SHARE: f64 = 0.7;
/// Shorter interims ("yeah", "okay so") are always shown: too little to judge.
const INTERIM_BLEED_MIN_TOKENS: usize = 3;

impl InterimBleedFilter {
    /// Remember far-side words (its interims and finals).
    fn heard_far(&mut self, text: &str, now_ms: f64) {
        self.far.retain(|_, at| now_ms - *at <= INTERIM_BLEED_WINDOW_MS);
        let words = word_list(text);
        let kept = words.iter().zip(&self.last_far).take_while(|(a, b)| a == b).count();
        for word in &words[kept..] {
            self.far.insert(word.clone(), now_ms);
        }
        self.last_far = words;
    }

    /// Whether a Mic-In interim is mostly words the far side just said.
    fn is_bleed(&mut self, text: &str, now_ms: f64) -> bool {
        self.far.retain(|_, at| now_ms - *at <= INTERIM_BLEED_WINDOW_MS);
        let mic = word_tokens(text);
        if mic.len() < INTERIM_BLEED_MIN_TOKENS {
            return false;
        }
        let echoed = mic.iter().filter(|t| self.far.contains_key(*t)).count();
        echoed as f64 >= INTERIM_BLEED_MIN_SHARE * mic.len() as f64
    }

    /// The interim to show: a Mic-In line found to be bleed becomes a clear.
    fn filter(&mut self, speaker: Option<&'static str>, text: String, now_ms: f64) -> RealtimeOutput {
        match speaker {
            Some(SPEAKER_OUT_LABEL) => self.heard_far(&text, now_ms),
            Some(MIC_IN_LABEL) if self.is_bleed(&text, now_ms) => {
                return RealtimeOutput::Interim { speaker, text: String::new() };
            }
            _ => {}
        }
        RealtimeOutput::Interim { speaker, text }
    }
}

/// What a lane reader hands the emitter, in arrival order, so a channel's
/// interim clear always follows the final that replaced it.
enum LaneEvent {
    Final(LabeledSegment),
    Interim { speaker: Option<&'static str>, text: String },
}

/// The transcript rows (sequence ids) behind each shown Mic-In line, kept as
/// long as the suppressor may still retract that line. One line can be
/// several rows when diarization split it.
#[derive(Default)]
struct RetractionLedger {
    rows: Vec<(f64, u64, Vec<u64>)>,
}

impl RetractionLedger {
    fn record(&mut self, now_ms: f64, key: u64, sequence_ids: Vec<u64>) {
        self.rows.retain(|(at, _, _)| now_ms - at <= BLEED_MEMORY_MS);
        self.rows.push((now_ms, key, sequence_ids));
    }

    /// The rows to remove for a retracted line (empty if unknown or expired).
    fn take(&mut self, key: u64) -> Vec<u64> {
        match self.rows.iter().position(|(_, k, _)| *k == key) {
            Some(i) => self.rows.remove(i).2,
            None => Vec::new(),
        }
    }
}

/// Removes Mic-In segments that duplicate a Speaker-Out segment without
/// delaying the user's own speech. DO NOT hold Mic-In finals until the
/// Speaker-Out channel catches up: that hold cost ~4s per mic line live
/// (Jake, 2026-10-15: "the latency of mic-in is unacceptable"). Instead:
/// - a Mic-In final matching a remembered Speaker-Out final is never shown;
/// - otherwise it is shown at once, and retracted if a matching Speaker-Out
///   final arrives later (bleed then flickers briefly; real speech never waits).
/// Pure: the caller supplies a monotonic clock.
#[derive(Default)]
struct BleedSuppressor {
    next_key: u64,
    shown_mic: Vec<(f64, u64, LabeledSegment)>,
    recent_speaker: Vec<(f64, LabeledSegment)>,
}

impl BleedSuppressor {
    fn push(&mut self, seg: LabeledSegment, now_ms: f64) -> Vec<RealtimeOutput> {
        self.shown_mic.retain(|(at, _, _)| now_ms - at <= BLEED_MEMORY_MS);
        self.recent_speaker.retain(|(at, _)| now_ms - at <= BLEED_MEMORY_MS);
        let key = self.next_key;
        self.next_key += 1;
        match seg.speaker {
            Some(MIC_IN_LABEL) => {
                let far: Vec<&LabeledSegment> = self.recent_speaker.iter().map(|(_, s)| s).collect();
                let seg = match judge_mic(&seg, &far) {
                    MicVerdict::Drop => return Vec::new(),
                    MicVerdict::Trim(keep) => keep,
                    MicVerdict::Keep => seg,
                };
                self.shown_mic.push((now_ms, key, seg.clone()));
                vec![RealtimeOutput::Show { key, seg }]
            }
            Some(SPEAKER_OUT_LABEL) => {
                let out_seg = seg.clone();
                self.recent_speaker.push((now_ms, seg));
                let far: Vec<&LabeledSegment> = self.recent_speaker.iter().map(|(_, s)| s).collect();
                let mut out = vec![RealtimeOutput::Show { key, seg: out_seg }];
                let mut reshown = Vec::new();
                self.shown_mic.retain(|(at, mic_key, mic)| match judge_mic(mic, &far) {
                    MicVerdict::Keep => true,
                    MicVerdict::Drop => {
                        out.push(RealtimeOutput::Retract { key: *mic_key });
                        false
                    }
                    MicVerdict::Trim(kept) => {
                        // Part of a shown line was bleed: take the line back
                        // and show what the user actually said.
                        out.push(RealtimeOutput::Retract { key: *mic_key });
                        reshown.push((*at, kept));
                        false
                    }
                });
                for (at, kept) in reshown {
                    let kept_key = self.next_key;
                    self.next_key += 1;
                    self.shown_mic.push((at, kept_key, kept.clone()));
                    out.push(RealtimeOutput::Show { key: kept_key, seg: kept });
                }
                out
            }
            _ => vec![RealtimeOutput::Show { key, seg }],
        }
    }
}

// ============================================================================
// REALTIME (WebSocket)
// ============================================================================

/// Where the realtime sockets connect. Production uses `DEEPGRAM`; tests point
/// it at a local server to exercise failure paths.
#[derive(Debug, Clone)]
pub(crate) struct RealtimeEndpoints {
    pub(crate) nova: String,
    pub(crate) flux: String,
    /// Add `side=me|them|unknown` to each Flux lane's URL. Only the Pipecat
    /// sidecar reads it (it runs live assist on the "them" lane); Deepgram
    /// itself answers HTTP 400 to unknown query parameters, so DO NOT set it
    /// for api.deepgram.com.
    pub(crate) tag_lane_side: bool,
}

impl RealtimeEndpoints {
    fn deepgram() -> Self {
        Self { nova: LISTEN_WS_URL.to_string(), flux: FLUX_WS_URL.to_string(), tag_lane_side: false }
    }
}

/// The `side` a lane's audio comes from, as the Pipecat sidecar names it.
fn lane_side(layout: AudioLayout, lane: Lane) -> &'static str {
    match channel_label(layout, lane.first_channel) {
        Some(MIC_IN_LABEL) => "me",
        Some(SPEAKER_OUT_LABEL) => "them",
        _ => "unknown",
    }
}

/// One chunk's worth of audio for one lane, plus where it sits in the recording.
struct LaneFrame {
    bytes: Vec<u8>,
    entry: AudioTimeMapEntry,
}

pub async fn run_realtime_transcription<R: Runtime>(
    app: AppHandle<R>,
    transcription_receiver: mpsc::UnboundedReceiver<AudioChunk>,
) -> Result<(), String> {
    REALTIME_SPEECH_DETECTED_EMITTED.store(false, Ordering::SeqCst);

    let api_key = configured_api_key(&app).await.map_err(|e| e.to_string())?;
    let options = configured_options(&app).await.map_err(|e| e.to_string())?;
    let language = crate::get_language_preference_internal();

    let (receiver, already_shown) = if super::pipecat::load_engine(&app) == super::pipecat::LiveEngine::Pipecat {
        match super::pipecat::run(&app, &api_key, &options, language.clone(), transcription_receiver).await {
            super::pipecat::Outcome::Finished(result) => return result,
            super::pipecat::Outcome::FallBack { receiver, reason, shown } => {
                warn!("Pipecat live engine: falling back to Direct for this recording: {reason}");
                let _ = app.emit(
                    "transcription-error",
                    serde_json::json!({
                        "error": reason,
                        "userMessage": format!(
                            "The Pipecat live engine stopped ({reason}). Transcribing with Direct ({}) for the rest of this recording.",
                            options.model
                        ),
                        "actionable": false
                    }),
                );
                (receiver, shown)
            }
        }
    } else {
        (transcription_receiver, ShownUntil::default())
    };

    run_realtime_core(
        RealtimeEndpoints::deepgram(),
        api_key,
        options,
        language,
        receiver,
        realtime_emitter(app, Arc::default(), already_shown),
    )
    .await
}

/// Recording time (ms) up to which each channel's lines have been shown.
/// After a fallback the Pipecat engine's unconfirmed audio is replayed to
/// Direct, and Direct's lines that end by then were already shown once.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ShownUntil {
    pub mic_in_ms: f64,
    pub speaker_out_ms: f64,
    pub unlabelled_ms: f64,
}

impl ShownUntil {
    fn slot(&mut self, speaker: Option<&str>) -> &mut f64 {
        match speaker {
            Some(MIC_IN_LABEL) => &mut self.mic_in_ms,
            Some(SPEAKER_OUT_LABEL) => &mut self.speaker_out_ms,
            _ => &mut self.unlabelled_ms,
        }
    }

    /// The part of a replayed line that was not shown yet: its words that
    /// start after what this channel already showed (plus REPLAY_JITTER_MS,
    /// since Direct's word timings run a little later than Flux's: measured
    /// 2026-11-18, two replayed lines ended a few hundred ms past the cutoff
    /// and showed twice when the whole line was compared). A line without
    /// word timings is kept when its midpoint is at or after the cutoff.
    /// Diarized runs are clipped with the words (`with_words`).
    fn unshown(mut self, seg: LabeledSegment) -> Option<LabeledSegment> {
        let until = *self.slot(seg.speaker);
        if until <= 0.0 {
            return Some(seg);
        }
        if seg.words.is_empty() {
            // At or after the cutoff: new. Before it: shown up to its middle.
            return ((seg.start_ms + seg.end_ms) / 2.0 >= until).then_some(seg);
        }
        if seg.words.iter().all(|w| w.start_ms >= until + REPLAY_JITTER_MS) {
            return Some(seg);
        }
        with_words(&seg, |w| w.start_ms >= until + REPLAY_JITTER_MS)
    }
}

/// See `ShownUntil::unshown`.
const REPLAY_JITTER_MS: f64 = 150.0;

/// Turns the core's output into transcript events: rows for shown lines,
/// `transcript-retract` for Mic-In lines found to be bleed. Records in
/// `shown` how far each channel got, and drops lines `skip` says were shown
/// already (a Direct fallback replaying the Pipecat engine's tail).
pub(crate) fn realtime_emitter<R: Runtime>(
    app: AppHandle<R>,
    shown: Arc<std::sync::Mutex<ShownUntil>>,
    skip: ShownUntil,
) -> impl FnMut(RealtimeOutput) + Send + 'static {
    // Only Mic-In lines are ever retracted, so only they are recorded.
    let mut ledger = RetractionLedger::default();
    let clock = std::time::Instant::now();
    move |out| match out {
        RealtimeOutput::Show { key, seg } => {
            let Some(seg) = skip.unshown(seg) else {
                info!("Deepgram realtime: skipping a replayed line already shown before the fallback");
                return;
            };
            {
                let mut shown = shown.lock().unwrap();
                let slot = shown.slot(seg.speaker);
                *slot = slot.max(seg.end_ms);
            }
            let retractable = seg.speaker == Some(MIC_IN_LABEL);
            let sequence_ids = emit_labeled_segment(&app, seg);
            if retractable {
                ledger.record(clock.elapsed().as_secs_f64() * 1000.0, key, sequence_ids);
            }
        }
        RealtimeOutput::Retract { key } => {
            for sequence_id in ledger.take(key) {
                emit_transcript_retract(&app, sequence_id);
            }
        }
        RealtimeOutput::Interim { speaker, text } => emit_transcript_interim(&app, speaker, text),
    }
}

/// The realtime session itself, free of Tauri state so it can be driven
/// directly (see `live_stereo_realtime_labels_each_channel`).
/// `emit` receives every finalized segment (and retraction) from bleed
/// suppression, as Deepgram finalizes it. It is
/// never called after this function returns, on success or error: the
/// worker emits `transcription-error` after an Err, and a late transcript
/// line after that would be out of order.
pub(crate) async fn run_realtime_core<F>(
    endpoints: RealtimeEndpoints,
    api_key: String,
    options: DeepgramOptions,
    language: Option<String>,
    transcription_receiver: mpsc::UnboundedReceiver<AudioChunk>,
    mut emit: F,
) -> Result<(), String>
where
    F: FnMut(RealtimeOutput) + Send + 'static,
{
    // The socket URL needs the channel count, and only the pipeline knows it,
    // so wait for the first audio before connecting. Audio queues in the
    // channel meanwhile; nothing is lost.
    let mut receiver = transcription_receiver;
    let first_chunk = loop {
        match receiver.recv().await {
            Some(chunk) if !chunk.data.is_empty() => break chunk,
            Some(_) => continue,
            None => {
                info!("Deepgram realtime: recording ended before any audio arrived");
                return Ok(());
            }
        }
    };
    let layout = first_chunk.layout;
    let input_channels = layout.channels() as usize;

    // Flux is a distinct realtime API: /v2/listen with turn-based `TurnInfo` events
    // and its own model-selected language handling. nova uses /v1/listen `Results`.
    let flux = is_flux_model(&options.model);
    let lanes = plan_lanes(flux, input_channels);
    info!(
        "Connecting to Deepgram realtime transcription ({}, {:?}, {} socket(s))",
        options.model,
        layout,
        lanes.len()
    );

    // Connect every lane before spawning anything, so a failed connection
    // (e.g. the second Flux socket) leaves no running task behind.
    let mut sockets = Vec::with_capacity(lanes.len());
    for lane in &lanes {
        let url = if flux {
            let url = build_flux_url(&endpoints.flux, &options, language.clone());
            if endpoints.tag_lane_side {
                format!("{}&side={}", url, lane_side(layout, *lane))
            } else {
                url
            }
        } else {
            build_realtime_url(&endpoints.nova, &options, language.clone(), lane.channels)
        };
        let mut request = url
            .into_client_request()
            .map_err(|e| format!("Failed to build Deepgram WebSocket request: {}", e))?;
        // Deepgram uses the "Token" scheme, not "Bearer".
        let auth_header = HeaderValue::from_str(&format!("Token {}", api_key))
            .map_err(|e| format!("Failed to build Deepgram auth header: {}", e))?;
        request.headers_mut().insert("Authorization", auth_header);
        let (ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| format!("Failed to connect to Deepgram realtime API: {}", e))?;
        sockets.push((*lane, ws_stream));
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<LaneEvent>();
    let mut lane_senders = Vec::with_capacity(lanes.len());
    let mut writer_handles = Vec::with_capacity(lanes.len());
    let mut reader_handles = Vec::with_capacity(lanes.len());

    for (lane, ws_stream) in sockets {
        let (mut write, mut read) = ws_stream.split();
        let time_map = Arc::new(Mutex::new(Vec::<AudioTimeMapEntry>::new()));
        let (lane_tx, mut lane_rx) = mpsc::unbounded_channel::<LaneFrame>();
        lane_senders.push((lane, lane_tx));

        let writer_time_map = time_map.clone();
        let writer_shutdown = shutdown.clone();
        writer_handles.push(tokio::spawn(async move {
            let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
            let mut shutdown_interval = tokio::time::interval(Duration::from_millis(250));

            loop {
                tokio::select! {
                    _ = shutdown_interval.tick() => {
                        if writer_shutdown.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    _ = keepalive.tick() => {
                        // nova closes an idle socket after ~10s; Flux manages turns itself
                        // and does not accept the KeepAlive control frame.
                        if !flux {
                            let msg = serde_json::json!({ "type": "KeepAlive" }).to_string();
                            if let Err(e) = write.send(Message::Text(msg)).await {
                                return Err(anyhow!("Failed to send Deepgram KeepAlive: {}", e));
                            }
                        }
                    }
                    maybe_frame = lane_rx.recv() => {
                        let Some(frame) = maybe_frame else {
                            break;
                        };
                        writer_time_map.lock().await.push(frame.entry);
                        if let Err(e) = write.send(Message::Binary(frame.bytes)).await {
                            return Err(anyhow!("Failed to send audio to Deepgram realtime API: {}", e));
                        }
                    }
                }
            }

            // Flux only ends a turn after it hears silence: neither CloseStream nor a
            // WebSocket close flushes the in-progress turn (verified live), and
            // dropping the write half leaves the socket open until Deepgram's 60s
            // no-ping timeout. So feed trailing silence at realtime pace first,
            // unless the session is already failing.
            if flux && !writer_shutdown.load(Ordering::SeqCst) {
                let silence = f32_to_pcm16le(&vec![0.0; FLUX_SILENCE_FRAME_SAMPLES * lane.channels]);
                for _ in 0..FLUX_TRAILING_SILENCE_FRAMES {
                    if write.send(Message::Binary(silence.clone())).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            // Both protocols then finalize and close on CloseStream.
            let close = serde_json::json!({ "type": "CloseStream" }).to_string();
            let _ = write.send(Message::Text(close)).await;
            Ok::<(), anyhow::Error>(())
        }));

        let reader_time_map = time_map;
        let reader_shutdown = shutdown.clone();
        let reader_events = event_tx.clone();
        reader_handles.push(tokio::spawn(async move {
            // nova only: each channel's last interim words (see interim_after).
            let mut interim_words: std::collections::HashMap<Option<usize>, Vec<DeepgramWord>> = Default::default();
            while let Some(message) = read.next().await {
                let message = match message {
                    Ok(message) => message,
                    Err(e) => {
                        reader_shutdown.store(true, Ordering::SeqCst);
                        return Err(anyhow!("Deepgram realtime WebSocket error: {}", e));
                    }
                };
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    continue;
                };

                // Reduce per protocol to (global channel, optional final,
                // where a nova final's audio ends), or send the interim text of
                // the words not final yet. Both report `type:"Error"` on failure.
                let (channel, final_seg, covered_until_s) = if flux {
                    let m = match serde_json::from_str::<FluxTurnInfo>(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("Failed to parse Deepgram Flux response: {} ({})", e, text);
                            continue;
                        }
                    };
                    if m.msg_type.as_deref() == Some("Error") {
                        let err = m.description.or(m.message).unwrap_or_else(|| "unknown error".to_string());
                        reader_shutdown.store(true, Ordering::SeqCst);
                        return Err(anyhow!("Deepgram Flux realtime error: {}", err));
                    }
                    if m.msg_type.as_deref() != Some("TurnInfo") {
                        continue;
                    }
                    let channel = resolve_channel(lane, None);
                    if let Some(text) = flux_turn_to_interim(&m) {
                        let speaker = channel.and_then(|c| channel_label(layout, c));
                        let _ = reader_events.send(LaneEvent::Interim { speaker, text });
                        continue;
                    }
                    if m.event.as_deref() != Some("EndOfTurn") {
                        // An empty Update (Flux sends ~4/s through silence)
                        // is neither a final nor anything to clear.
                        continue;
                    }
                    (channel, flux_turn_to_final(&m), None)
                } else {
                    let m = match serde_json::from_str::<RealtimeMessage>(&text) {
                        Ok(m) => m,
                        Err(e) => {
                            warn!("Failed to parse Deepgram realtime response: {} ({})", e, text);
                            continue;
                        }
                    };
                    if m.msg_type.as_deref() == Some("Error") {
                        let err = m.description.or(m.message).unwrap_or_else(|| "unknown error".to_string());
                        reader_shutdown.store(true, Ordering::SeqCst);
                        return Err(anyhow!("Deepgram realtime error: {}", err));
                    }
                    if let Some((reported, text, words)) = nova_interim(&m) {
                        let channel = resolve_channel(lane, reported);
                        interim_words.insert(channel, words);
                        let speaker = channel.and_then(|c| channel_label(layout, c));
                        let _ = reader_events.send(LaneEvent::Interim { speaker, text });
                        continue;
                    }
                    let covered_until_s = m.start + m.duration;
                    let Some(fin) = nova_final(m) else { continue };
                    let channel = resolve_channel(lane, fin.channel_index);
                    if channel.is_none() {
                        warn!(
                            "Deepgram realtime: Results on a {}-channel socket with channel_index {:?}; emitting unlabelled",
                            lane.channels, fin.channel_index
                        );
                    }
                    (channel, fin.seg, Some(covered_until_s))
                };

                let speaker = channel.and_then(|c| channel_label(layout, c));
                let entries = reader_time_map.lock().await.clone();
                if let Some(seg) = final_seg {
                    let to_original = |stream_start: f64, stream_end: f64| {
                        let start = map_stream_ms_to_original(&entries, stream_start);
                        (start, map_stream_ms_to_original(&entries, stream_end).max(start))
                    };
                    let (start_ms, end_ms) = to_original(seg.start_ms, seg.end_ms);
                    let turns = seg
                        .turns
                        .into_iter()
                        .map(|t| {
                            let (start_ms, end_ms) = to_original(t.start_ms, t.end_ms);
                            SpeakerTurn { start_ms, end_ms, ..t }
                        })
                        .collect();
                    let words = seg
                        .words
                        .into_iter()
                        .map(|w| {
                            let (start_ms, end_ms) = to_original(w.start_ms, w.end_ms);
                            TimedWord { start_ms, end_ms, ..w }
                        })
                        .collect();
                    let _ = reader_events.send(LaneEvent::Final(LabeledSegment {
                        text: seg.transcript,
                        start_ms,
                        end_ms,
                        confidence: seg.confidence,
                        speaker,
                        turns,
                        words,
                    }));
                }
                // A final (even an empty one) replaces the channel's interim
                // words it covers; sent after it so the line never blinks out
                // first. A Flux EndOfTurn covers its whole turn.
                let rest = match covered_until_s {
                    Some(end_s) => interim_after(interim_words.get(&channel).map_or(&[][..], Vec::as_slice), end_s),
                    None => Vec::new(),
                };
                let text = words_line(&rest);
                interim_words.insert(channel, rest);
                let _ = reader_events.send(LaneEvent::Interim { speaker, text });
            }

            reader_shutdown.store(true, Ordering::SeqCst);
            Ok::<(), anyhow::Error>(())
        }));
    }
    // Only the readers hold senders now, so the emitter ends when they do.
    drop(event_tx);

    // Every final goes out as it arrives. Without a Speaker-Out channel
    // (mic-only or mono) the suppressor never matches, so it only assigns keys.
    let emitter_handle = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut suppressor = BleedSuppressor::default();
        let mut interim_bleed = InterimBleedFilter::default();
        // Channels with greyed words on screen.
        let mut greyed: std::collections::HashSet<Option<&'static str>> = Default::default();
        while let Some(event) = event_rx.recv().await {
            let now_ms = started.elapsed().as_secs_f64() * 1000.0;
            match event {
                LaneEvent::Final(seg) => {
                    if seg.speaker == Some(SPEAKER_OUT_LABEL) {
                        interim_bleed.heard_far(&seg.text, now_ms);
                    }
                    for out in suppressor.push(seg, now_ms) {
                        emit(out);
                    }
                }
                LaneEvent::Interim { speaker, text } => {
                    let out = interim_bleed.filter(speaker, text, now_ms);
                    if let RealtimeOutput::Interim { speaker, text } = &out {
                        if text.is_empty() {
                            greyed.remove(speaker);
                        } else {
                            greyed.insert(*speaker);
                        }
                    }
                    emit(out);
                }
            }
        }
        // The session is over (stop, error, or a Pipecat failure handing over
        // to Direct): no final will replace what is still greyed, so clear it.
        for speaker in greyed {
            emit(RealtimeOutput::Interim { speaker, text: String::new() });
        }
    });

    // Dispatcher: fan each pipeline chunk out to every lane's socket.
    let mut first_error: Option<String> = None;
    {
        let mut cursors_ms = vec![0.0f64; lane_senders.len()];
        let mut next = Some(first_chunk);
        let mut shutdown_check = tokio::time::interval(Duration::from_millis(250));
        loop {
            let chunk = match next.take() {
                Some(chunk) => chunk,
                None => tokio::select! {
                    maybe_chunk = receiver.recv() => match maybe_chunk {
                        Some(chunk) => chunk,
                        None => break,
                    },
                    _ = shutdown_check.tick() => {
                        if shutdown.load(Ordering::SeqCst) {
                            break;
                        }
                        continue;
                    }
                },
            };
            if chunk.data.is_empty() {
                continue;
            }
            if chunk.layout != layout {
                warn!(
                    "Deepgram realtime: dropping {:?} chunk on a {:?} stream",
                    chunk.layout, layout
                );
                continue;
            }

            let original_start_ms = chunk.timestamp * 1000.0;
            let original_end_ms = original_start_ms + chunk_duration_ms(&chunk);

            for ((lane, lane_tx), cursor) in lane_senders.iter().zip(cursors_ms.iter_mut()) {
                let Some(samples) = lane_samples(&chunk, *lane) else {
                    continue;
                };
                if samples.is_empty() {
                    continue;
                }
                let frames = samples.len() / lane.channels;
                let duration_ms = frames as f64 / PCM_SAMPLE_RATE as f64 * 1000.0;
                let entry = AudioTimeMapEntry {
                    stream_start_ms: *cursor,
                    stream_end_ms: *cursor + duration_ms,
                    original_start_ms,
                    original_end_ms,
                };
                *cursor += duration_ms;
                if lane_tx
                    .send(LaneFrame { bytes: f32_to_pcm16le(&samples), entry })
                    .is_err()
                {
                    // The lane's writer already exited; its error surfaces below.
                    shutdown.store(true, Ordering::SeqCst);
                }
            }
        }
    }
    // Closing the lane senders tells each writer to finalize its socket.
    drop(lane_senders);

    for writer_handle in writer_handles {
        match writer_handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                first_error.get_or_insert_with(|| e.to_string());
            }
            Err(e) => {
                first_error.get_or_insert_with(|| format!("Deepgram realtime writer task failed: {}", e));
            }
        }
        if first_error.is_some() {
            shutdown.store(true, Ordering::SeqCst);
        }
    }

    // Readers: on success, give the final responses a shared 30s deadline
    // (waiting 30s per socket in turn would stack). On any error, stop every
    // reader now; nothing they would still produce may reach `emit`.
    let reader_aborts: Vec<_> = reader_handles.iter().map(|h| h.abort_handle()).collect();
    if first_error.is_some() {
        reader_aborts.iter().for_each(|h| h.abort());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    for mut reader_handle in reader_handles {
        match tokio::time::timeout_at(deadline, &mut reader_handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                first_error.get_or_insert_with(|| e.to_string());
                reader_aborts.iter().for_each(|h| h.abort());
            }
            Ok(Err(e)) if e.is_cancelled() => {}
            Ok(Err(e)) => {
                first_error.get_or_insert_with(|| format!("Deepgram realtime reader task failed: {}", e));
                reader_aborts.iter().for_each(|h| h.abort());
            }
            Err(_) => {
                warn!("Timed out waiting for Deepgram realtime final response");
                reader_handle.abort();
                let _ = reader_handle.await;
            }
        }
    }

    // Every reader has ended, so the event channel is closed: the emitter
    // finishes the finals already queued and exits. Await it so no emit can
    // happen after we return.
    let _ = emitter_handle.await;

    match first_error {
        Some(e) => Err(e),
        None => {
            info!("Deepgram realtime transcription task completed");
            Ok(())
        }
    }
}

/// Emits the segment's transcript rows; returns their sequence ids.
fn emit_labeled_segment<R: Runtime>(app: &AppHandle<R>, seg: LabeledSegment) -> Vec<u64> {
    emit_speech_detected(app);
    transcript_lines(seg)
        .into_iter()
        .map(|line| {
            emit_transcript_update(app, line.text, line.start_ms, line.end_ms, line.confidence, false, line.speaker)
        })
        .collect()
}

/// Event that removes an already-emitted transcript row (payload
/// `{ "sequence_id": n }`). DO NOT rename without the listeners in
/// recording_commands.rs and frontend src/services/transcriptService.ts
/// (`ts_listens_for_transcript_retract` checks the TS side).
pub const TRANSCRIPT_RETRACT_EVENT: &str = "transcript-retract";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptRetract {
    pub sequence_id: u64,
}

fn emit_transcript_retract<R: Runtime>(app: &AppHandle<R>, sequence_id: u64) {
    if let Err(e) = app.emit(TRANSCRIPT_RETRACT_EVENT, TranscriptRetract { sequence_id }) {
        error!("Failed to emit transcript retraction: {}", e);
    }
}

/// Event carrying a channel's not-yet-final words (payload
/// `{ "speaker": "Mic-In" | "Speaker-Out" | null, "text": "..." }`, empty
/// text clears that channel). Display only: nothing stores it. DO NOT rename
/// without frontend src/lib/transcript-interim.ts
/// (`ts_listens_for_transcript_interim` checks).
pub const TRANSCRIPT_INTERIM_EVENT: &str = "transcript-interim";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptInterim {
    pub speaker: Option<String>,
    pub text: String,
}

fn emit_transcript_interim<R: Runtime>(app: &AppHandle<R>, speaker: Option<&'static str>, text: String) {
    let payload = TranscriptInterim { speaker: speaker.map(str::to_string), text };
    if let Err(e) = app.emit(TRANSCRIPT_INTERIM_EVENT, payload) {
        error!("Failed to emit interim transcript: {}", e);
    }
}

/// One stored transcript row.
#[derive(Debug, PartialEq)]
struct TranscriptLine {
    text: String,
    start_ms: f64,
    end_ms: f64,
    confidence: f32,
    speaker: Option<String>,
}

/// Rows for one finalized segment: one per diarized speaker run when the
/// final holds several voices, otherwise the whole segment (numbered when
/// diarization identified its single speaker).
fn transcript_lines(seg: LabeledSegment) -> Vec<TranscriptLine> {
    if seg.turns.len() > 1 {
        return seg
            .turns
            .into_iter()
            .map(|t| TranscriptLine {
                text: t.text,
                start_ms: t.start_ms,
                end_ms: t.end_ms,
                confidence: t.confidence,
                speaker: segment_label(seg.speaker, Some(t.speaker)),
            })
            .collect();
    }
    vec![TranscriptLine {
        speaker: segment_label(seg.speaker, seg.turns.first().map(|t| t.speaker)),
        text: seg.text,
        start_ms: seg.start_ms,
        end_ms: seg.end_ms,
        confidence: seg.confidence,
    }]
}

/// The stored/displayed speaker for a segment. The channel label says which
/// side spoke; diarization (opt-in) further numbers the voices on that
/// channel, 1-based: "Speaker-Out 2". A diarized mono stream is "Speaker 1".
fn segment_label(source: Option<&str>, diarized: Option<i64>) -> Option<String> {
    match (source, diarized) {
        (Some(src), Some(n)) => Some(format!("{} {}", src, n + 1)),
        (Some(src), None) => Some(src.to_string()),
        (None, Some(n)) => Some(format!("Speaker {}", n + 1)),
        (None, None) => None,
    }
}

pub fn map_stream_ms_to_original(entries: &[AudioTimeMapEntry], stream_ms: f64) -> f64 {
    if entries.is_empty() {
        return stream_ms;
    }

    if let Some(entry) = entries
        .iter()
        .find(|entry| stream_ms >= entry.stream_start_ms && stream_ms <= entry.stream_end_ms)
    {
        let stream_duration = (entry.stream_end_ms - entry.stream_start_ms).max(1.0);
        let original_duration = (entry.original_end_ms - entry.original_start_ms).max(0.0);
        let offset_ratio = ((stream_ms - entry.stream_start_ms) / stream_duration).clamp(0.0, 1.0);
        return entry.original_start_ms + original_duration * offset_ratio;
    }

    if stream_ms < entries[0].stream_start_ms {
        return entries[0].original_start_ms;
    }

    let last = entries.last().expect("entries is not empty");
    last.original_end_ms + (stream_ms - last.stream_end_ms).max(0.0)
}

/// True for Flux models (`flux-general-en`, `flux-general-multi`), which use the
/// separate v2 turn-based realtime API rather than the nova `/v1/listen` path.
fn is_flux_model(model: &str) -> bool {
    model.starts_with("flux")
}

fn build_realtime_url(base: &str, options: &DeepgramOptions, language: Option<String>, channels: usize) -> String {
    let channels = channels.max(1);
    let mut query: Vec<(String, String)> = vec![
        ("encoding".to_string(), "linear16".to_string()),
        ("sample_rate".to_string(), PCM_SAMPLE_RATE.to_string()),
        ("channels".to_string(), channels.to_string()),
        ("model".to_string(), options.model.clone()),
        ("smart_format".to_string(), "true".to_string()),
        ("punctuate".to_string(), "true".to_string()),
        // Interims feed only the greyed live line (`transcript-interim`).
        // DO NOT route them through `transcript-update`: the transcript buffers
        // rows by sequence id with no partial-replace path, so every interim
        // would become a duplicate saved row. Finals are unchanged by this
        // flag (same `is_final` results, same billing).
        ("interim_results".to_string(), "true".to_string()),
        ("endpointing".to_string(), "300".to_string()),
    ];
    if channels > 1 {
        // Transcribe each channel independently (results carry `channel_index`)
        // instead of downmixing; verified live with nova-3 streaming.
        query.push(("multichannel".to_string(), "true".to_string()));
    }
    if options.diarize {
        // diarize_model=latest replaces diarize=true (400 if both sent). See
        // prerecorded_query; verified live that streaming accepts it (101 handshake).
        query.push(("diarize_model".to_string(), "latest".to_string()));
    }
    if options.mip_opt_out {
        query.push(("mip_opt_out".to_string(), "true".to_string()));
    }
    // Deepgram-specific language wins; fall back to the caller's hint if unset.
    let lang = options.language.as_deref().or(language.as_deref());
    apply_language(&mut query, lang, &options.model);
    apply_keyterms(&mut query, &options.keyterms);
    encode_ws_url(base, &query)
}

/// Build the Flux v2 realtime URL. Flux does its own turn detection and
/// formatting, so it takes none of the nova knobs (smart_format/punctuate/
/// diarize/endpointing/language). The multilingual model accepts `language_hint`;
/// language is otherwise selected by the model name, not a `language` param.
fn build_flux_url(base: &str, options: &DeepgramOptions, language: Option<String>) -> String {
    let mut query: Vec<(String, String)> = vec![
        ("model".to_string(), options.model.clone()),
        ("encoding".to_string(), "linear16".to_string()),
        ("sample_rate".to_string(), PCM_SAMPLE_RATE.to_string()),
    ];
    if options.mip_opt_out {
        // Flux takes the same Model Improvement Program opt-out as nova
        // (verified live 2026-11-18: the handshake accepts it). Before
        // WORK-559 this was missing, so Flux recordings ignored the
        // privacy-first default.
        query.push(("mip_opt_out".to_string(), "true".to_string()));
    }
    if options.model.ends_with("multi") {
        if let Some(hint) = language.as_deref().map(str::trim).filter(|l| {
            !l.is_empty() && *l != "auto" && *l != "auto-translate" && *l != "detect"
        }) {
            query.push(("language_hint".to_string(), hint.to_string()));
        }
    }
    apply_keyterms(&mut query, &options.keyterms);
    encode_ws_url(base, &query)
}

fn encode_ws_url(base: &str, query: &[(String, String)]) -> String {
    let encoded = query
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{}", base, encoded)
}

// ============================================================================
// EMITTERS (shared with the local-engine worker event contract)
// ============================================================================

fn emit_speech_detected<R: Runtime>(app: &AppHandle<R>) {
    if REALTIME_SPEECH_DETECTED_EMITTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        let _ = app.emit(
            "speech-detected",
            serde_json::json!({ "message": "Speech activity detected" }),
        );
    }
}

fn emit_transcript_update<R: Runtime>(
    app: &AppHandle<R>,
    text: String,
    start_ms: f64,
    end_ms: f64,
    confidence: f32,
    is_partial: bool,
    speaker: Option<String>,
) -> u64 {
    let sequence_id = REALTIME_SEQUENCE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let audio_start_time = start_ms / 1000.0;
    let audio_end_time = end_ms.max(start_ms) / 1000.0;
    let duration = (audio_end_time - audio_start_time).max(0.0);

    let update = TranscriptUpdate {
        text,
        timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        source: "Audio".to_string(),
        sequence_id,
        chunk_start_time: audio_start_time,
        is_partial,
        confidence,
        audio_start_time,
        audio_end_time,
        duration,
        speaker,
    };

    if let Err(e) = app.emit("transcript-update", &update) {
        error!("Failed to emit Deepgram transcript update: {}", e);
    }
    sequence_id
}

// ============================================================================
// HELPERS
// ============================================================================

/// Map the stored language preference to Deepgram query params. Three distinct modes:
///   `detect`    -> `detect_language=true`  (auto-detect a single language)
///   `multi`     -> `language=multi`        (nova-3 code-switching across 10 languages)
///   ISO code    -> `language=<code>`       (explicit selection, e.g. `es`)
/// When unset/auto, nova-3 defaults to `multi` (its code-switching default) and other
/// models fall back to Deepgram's own default. Previously `detect` was silently dropped
/// and auto collapsed to `multi`, so single-language detection was unreachable.
fn apply_language(query: &mut Vec<(String, String)>, language: Option<&str>, model: &str) {
    match language.map(str::trim).unwrap_or("") {
        "detect" => query.push(("detect_language".to_string(), "true".to_string())),
        "multi" => query.push(("language".to_string(), "multi".to_string())),
        "" | "auto" | "auto-translate" => {
            if model.starts_with("nova-3") {
                query.push(("language".to_string(), "multi".to_string()));
            }
        }
        lang => query.push(("language".to_string(), lang.to_string())),
    }
}

/// keyterm prompting. The only supported models (nova-3 and Flux) both use the
/// `keyterm` parameter, so every term routes there. (nova-2's `keywords` param was
/// dropped along with nova-2 model support.)
fn apply_keyterms(query: &mut Vec<(String, String)>, keyterms: &[String]) {
    if keyterms.is_empty() {
        return;
    }
    for term in keyterms {
        let term = term.trim();
        if !term.is_empty() {
            query.push(("keyterm".to_string(), term.to_string()));
        }
    }
}

/// Split a stored keyterm blob (newline- or comma-separated) into terms.
pub fn parse_keyterms(raw: Option<&str>) -> Vec<String> {
    raw.map(|raw| {
        raw.split(['\n', ','])
            .map(str::trim)
            .filter(|term| !term.is_empty())
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "mp4") => "audio/mp4",
        Some("flac") => "audio/flac",
        Some("ogg" | "opus") => "audio/ogg",
        Some("webm") => "audio/webm",
        _ => "application/octet-stream",
    }
}

fn f32_to_pcm16le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let pcm = (clamped * i16::MAX as f32) as i16;
        bytes.extend_from_slice(&pcm.to_le_bytes());
    }
    bytes
}

async fn ensure_success(response: reqwest::Response, action: &str) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(anyhow!(
        "Deepgram {} failed (HTTP {}): {}",
        action,
        status,
        truncate_for_error(&body)
    ))
}

fn ensure_not_cancelled<C>(should_cancel: &mut C) -> Result<()>
where
    C: FnMut() -> bool,
{
    if should_cancel() {
        Err(anyhow!("Operation cancelled"))
    } else {
        Ok(())
    }
}

fn push_tuple(segments: &mut Vec<SpeakerTuple>, text: String, start_ms: f64, end_ms: f64, speaker: Option<i64>) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    segments.push((trimmed.to_string(), start_ms, end_ms.max(start_ms), speaker));
}

fn average_confidence(values: impl Iterator<Item = f32>) -> f32 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for value in values {
        sum += value;
        count += 1;
    }
    if count == 0 {
        0.85
    } else {
        sum / count as f32
    }
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .chars()
        .last()
        .map(|c| matches!(c, '.' | '!' | '?' | '\n'))
        .unwrap_or(false)
}

fn truncate_for_error(text: &str) -> String {
    const MAX_LEN: usize = 500;
    if text.len() <= MAX_LEN {
        return text.to_string();
    }
    let mut end = MAX_LEN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Minimal percent-encoding for query values (keyterms may contain spaces).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(w: &str, start: f64, end: f64, speaker: Option<i64>) -> DeepgramWord {
        DeepgramWord {
            word: w.to_string(),
            punctuated_word: Some(w.to_string()),
            start,
            end,
            confidence: Some(0.9),
            speaker,
        }
    }

    #[test]
    fn utterances_map_to_one_segment_each() {
        let transcript = DeepgramTranscript {
            text: "ignored".to_string(),
            utterances: vec![
                Utterance {
                    start: 0.0,
                    end: 1.5,
                    transcript: "Hello there.".to_string(),
                    speaker: None,
                },
                Utterance {
                    start: 2.0,
                    end: 3.0,
                    transcript: "General Kenobi.".to_string(),
                    speaker: None,
                },
            ],
            words: vec![],
            duration_seconds: Some(3.0),
        };
        let segments = transcript_to_segments(&transcript, 3.0);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "Hello there.");
        assert_eq!(segments[1].text, "General Kenobi.");
        assert_eq!(segments[0].audio_start_time, Some(0.0));
        assert_eq!(segments[1].audio_start_time, Some(2.0));
        assert_eq!(segments[0].speaker, None);
    }

    #[test]
    fn diarized_prerecorded_utterances_keep_numbered_speakers() {
        let json = r#"{ "metadata": { "duration": 3.0 }, "results": {
            "channels": [ { "alternatives": [ { "transcript": "Hello there. General Kenobi.", "words": [] } ] } ],
            "utterances": [
                { "start": 0.0, "end": 1.5, "transcript": "Hello there.", "speaker": 0 },
                { "start": 1.6, "end": 1.7, "transcript": "  ", "speaker": 2 },
                { "start": 2.0, "end": 3.0, "transcript": "General Kenobi.", "speaker": 1 }
            ] } }"#;
        let transcript = DeepgramTranscript::from(serde_json::from_str::<PrerecordedResponse>(json).unwrap());
        let segments = transcript_to_segments(&transcript, 3.0);
        let labels: Vec<(&str, Option<&str>)> =
            segments.iter().map(|s| (s.text.as_str(), s.speaker.as_deref())).collect();
        // The blank utterance is dropped without shifting later labels.
        assert_eq!(labels, vec![("Hello there.", Some("Speaker 1")), ("General Kenobi.", Some("Speaker 2"))]);
    }

    #[test]
    fn words_split_on_speaker_change() {
        let words = vec![
            word("Hello", 0.0, 0.4, Some(0)),
            word("world.", 0.4, 0.9, Some(0)),
            word("Reply", 1.0, 1.4, Some(1)),
        ];
        let tuples = words_to_tuples(&words);
        assert_eq!(tuples.len(), 2);
        assert_eq!(tuples[0].0, "Hello world.");
        assert_eq!(tuples[0].3, Some(0));
        assert_eq!(tuples[1].0, "Reply");
        assert_eq!(tuples[1].3, Some(1));
    }

    #[test]
    fn falls_back_to_single_segment_when_only_text() {
        let transcript = DeepgramTranscript {
            text: "Whole thing.".to_string(),
            utterances: vec![],
            words: vec![],
            duration_seconds: Some(5.0),
        };
        let segments = transcript_to_segments(&transcript, 5.0);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Whole thing.");
        assert_eq!(segments[0].audio_end_time, Some(5.0));
    }

    #[test]
    fn maps_stream_time_to_original_chunk_time() {
        let entries = vec![
            AudioTimeMapEntry {
                stream_start_ms: 0.0,
                stream_end_ms: 1000.0,
                original_start_ms: 5000.0,
                original_end_ms: 6000.0,
            },
            AudioTimeMapEntry {
                stream_start_ms: 1000.0,
                stream_end_ms: 2000.0,
                original_start_ms: 9000.0,
                original_end_ms: 10_000.0,
            },
        ];
        assert_eq!(map_stream_ms_to_original(&entries, 500.0), 5500.0);
        assert_eq!(map_stream_ms_to_original(&entries, 1500.0), 9500.0);
    }

    #[test]
    fn nova3_keyterms_use_keyterm_param() {
        let mut query = vec![("model".to_string(), "nova-3".to_string())];
        apply_keyterms(&mut query, &["Meetily".to_string(), "Deepgram".to_string()]);
        let keyterms: Vec<_> = query.iter().filter(|(k, _)| k == "keyterm").collect();
        assert_eq!(keyterms.len(), 2);
    }

    #[test]
    fn language_defaults_to_multi_for_nova3_only() {
        let mut q1 = vec![];
        apply_language(&mut q1, None, "nova-3");
        assert_eq!(q1, vec![("language".to_string(), "multi".to_string())]);

        let mut q2 = vec![];
        apply_language(&mut q2, None, "nova-2");
        assert!(q2.is_empty());

        let mut q3 = vec![];
        apply_language(&mut q3, Some("en"), "nova-3");
        assert_eq!(q3, vec![("language".to_string(), "en".to_string())]);

        // `detect` -> single-language auto-detect; `multi` -> code-switching (any model).
        let mut q4 = vec![];
        apply_language(&mut q4, Some("detect"), "nova-3");
        assert_eq!(q4, vec![("detect_language".to_string(), "true".to_string())]);

        let mut q5 = vec![];
        apply_language(&mut q5, Some("multi"), "nova-2");
        assert_eq!(q5, vec![("language".to_string(), "multi".to_string())]);
    }

    #[test]
    fn parse_keyterms_splits_on_commas_and_newlines() {
        assert_eq!(
            parse_keyterms(Some("Meetily, Deepgram\nNova")),
            vec!["Meetily", "Deepgram", "Nova"]
        );
        assert!(parse_keyterms(None).is_empty());
        assert!(parse_keyterms(Some("  ,\n ")).is_empty());
    }

    #[test]
    fn realtime_url_contains_expected_params() {
        let options = DeepgramOptions {
            model: "nova-3".to_string(),
            keyterms: vec!["Meetily".to_string()],
            diarize: true,
            mip_opt_out: true,
            language: None,
        };
        let url = build_realtime_url(LISTEN_WS_URL, &options, Some("en".to_string()), 1);
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"));
        assert!(url.contains("encoding=linear16"));
        assert!(url.contains("model=nova-3"));
        assert!(url.contains("diarize_model=latest"));
        assert!(!url.contains("diarize=true")); // diarize_model replaces diarize (400 if both)
        assert!(url.contains("mip_opt_out=true")); // privacy-first: opt out by default
        assert!(url.contains("interim_results=true"));
        assert!(url.contains("keyterm=Meetily"));
        assert!(url.contains("language=en"));
        // Mono socket: one channel and no multichannel split.
        assert!(url.contains("channels=1"));
        assert!(!url.contains("multichannel"));
    }

    #[test]
    fn prerecorded_query_emits_diarize_model_and_mip_opt_out() {
        let client = DeepgramClient::new(
            "k".to_string(),
            DeepgramOptions {
                model: "nova-3".to_string(),
                keyterms: vec![],
                diarize: true,
                mip_opt_out: true,
                language: None,
            },
        );
        let q = client.prerecorded_query(Some("es".to_string()));
        let has = |k: &str, v: &str| q.iter().any(|(a, b)| a == k && b == v);
        assert!(has("diarize_model", "latest"));
        assert!(!q.iter().any(|(k, _)| k == "diarize")); // never diarize=true (400 if both)
        assert!(has("mip_opt_out", "true"));
        assert!(has("smart_format", "true"));
        assert!(has("language", "es")); // caller hint used when options.language is unset
    }

    #[test]
    fn deepgram_specific_language_overrides_caller_hint() {
        let opts = DeepgramOptions {
            model: "nova-3".to_string(),
            keyterms: vec![],
            diarize: false,
            mip_opt_out: false,
            language: Some("multi".to_string()),
        };
        // Even when the caller passes a global hint ("es"), the Deepgram-specific
        // language ("multi") wins, so global values never override the provider setting.
        let url = build_realtime_url(LISTEN_WS_URL, &opts, Some("es".to_string()), 1);
        assert!(url.contains("language=multi"));
        assert!(!url.contains("language=es"));
    }

    #[test]
    fn language_auto_is_treated_as_no_hint() {
        // "auto" is treated as no explicit hint; nova-3 then falls back to multi.
        let mut q = vec![];
        apply_language(&mut q, Some("auto"), "nova-3");
        assert_eq!(q, vec![("language".to_string(), "multi".to_string())]);
    }

    #[test]
    fn keyterm_with_spaces_is_percent_encoded_in_url() {
        let options = DeepgramOptions {
            model: "nova-3".to_string(),
            keyterms: vec!["Acme Corp".to_string()],
            diarize: false,
            mip_opt_out: false,
            language: None,
        };
        let url = build_realtime_url(LISTEN_WS_URL, &options, None, 1);
        assert!(url.contains("keyterm=Acme%20Corp"));
        assert!(!url.contains("diarize="));
        assert!(!url.contains("mip_opt_out")); // off -> param omitted
    }

    // Contract tests: lock the Deepgram wire shape we verified live so a
    // response-schema change is caught here rather than at runtime.
    #[test]
    fn parses_prerecorded_utterances_into_segments() {
        let json = r#"{
            "metadata": { "duration": 3.5 },
            "results": {
                "channels": [ { "alternatives": [ {
                    "transcript": "Hello there. General Kenobi.",
                    "confidence": 0.98,
                    "words": [ { "word": "hello", "punctuated_word": "Hello.",
                        "start": 0.0, "end": 0.8, "confidence": 0.95, "speaker": 0 } ]
                } ] } ],
                "utterances": [
                    { "start": 0.0, "end": 1.5, "confidence": 0.97,
                      "transcript": "Hello there.", "speaker": 0 },
                    { "start": 2.0, "end": 3.0, "confidence": 0.96,
                      "transcript": "General Kenobi.", "speaker": 1 }
                ]
            }
        }"#;
        let payload: PrerecordedResponse = serde_json::from_str(json).unwrap();
        let transcript = DeepgramTranscript::from(payload);
        assert_eq!(transcript.duration_seconds, Some(3.5));
        let segments = transcript_to_segments(&transcript, 3.5);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].text, "Hello there.");
        assert_eq!(segments[1].audio_start_time, Some(2.0));
    }

    #[test]
    fn parses_realtime_results_message() {
        let json = r#"{
            "type": "Results",
            "channel_index": [0, 1],
            "duration": 1.02,
            "start": 4.5,
            "is_final": true,
            "speech_final": true,
            "channel": { "alternatives": [ {
                "transcript": "This is a meeting.",
                "confidence": 0.99,
                "words": [ { "word": "this", "punctuated_word": "This",
                    "start": 4.5, "end": 4.7, "confidence": 0.99, "speaker": 0 } ]
            } ] }
        }"#;
        let msg: RealtimeMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type.as_deref(), Some("Results"));
        assert!(msg.is_final);
        assert_eq!(msg.start, 4.5);
        let alt = msg.channel.unwrap().alternatives.into_iter().next().unwrap();
        assert_eq!(alt.transcript, "This is a meeting.");
        assert_eq!(alt.confidence, Some(0.99));
    }

    #[test]
    fn parses_realtime_error_message() {
        let json = r#"{ "type": "Error", "description": "bad audio", "message": "detail" }"#;
        let msg: RealtimeMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type.as_deref(), Some("Error"));
        assert_eq!(msg.description.as_deref(), Some("bad audio"));
    }

    #[test]
    fn nova_result_reduction_finals_only() {
        let mk = |is_final: bool| RealtimeMessage {
            msg_type: Some("Results".to_string()),
            channel: Some(RealtimeChannel {
                alternatives: vec![Alternative {
                    transcript: "Hi there.".to_string(),
                    confidence: Some(0.9),
                    words: vec![],
                }],
            }),
            is_final,
            channel_index: serde_json::json!([0, 1]),
            start: 1.0,
            duration: 0.5,
            description: None,
            message: None,
        };
        assert!(nova_result_to_final(mk(false)).is_none());
        let seg = nova_result_to_final(mk(true)).unwrap();
        assert_eq!(seg.transcript, "Hi there.");
        assert_eq!(seg.start_ms, 1000.0);
        assert_eq!(seg.end_ms, 1500.0);
    }

    // --- Flux (v2) contract + reduction ---

    #[test]
    fn flux_interim_is_every_turn_event_but_end_of_turn() {
        let mk = |event: &str, transcript: &str| FluxTurnInfo {
            msg_type: Some("TurnInfo".to_string()),
            event: Some(event.to_string()),
            transcript: transcript.to_string(),
            audio_window_start: 0.0,
            audio_window_end: 1.0,
            words: Vec::new(),
            description: None,
            message: None,
        };
        for event in ["Update", "StartOfTurn", "EagerEndOfTurn", "TurnResumed"] {
            assert_eq!(flux_turn_to_interim(&mk(event, " so the ")).as_deref(), Some("so the"), "{event}");
        }
        assert_eq!(flux_turn_to_interim(&mk("EndOfTurn", "So the plan.")), None);
        assert_eq!(flux_turn_to_interim(&mk("StartOfTurn", "  ")), None, "no words yet: nothing to show");
    }

    #[test]
    fn nova_interim_is_a_non_final_result_with_text() {
        let msg = |is_final: bool, transcript: &str, channel_index: &str| -> RealtimeMessage {
            serde_json::from_str(&format!(
                r#"{{ "type": "Results", "is_final": {is_final}, "channel_index": {channel_index},
                    "channel": {{ "alternatives": [ {{ "transcript": "{transcript}", "words": [] }} ] }} }}"#
            ))
            .unwrap()
        };
        let text = |m: RealtimeMessage| nova_interim(&m).map(|(c, t, _)| (c, t));
        assert_eq!(text(msg(false, "we handle about", "[1, 2]")), Some((Some((1, 2)), "we handle about".to_string())));
        assert_eq!(text(msg(true, "We handle about forty.", "[1, 2]")), None, "finals take the final path");
        assert_eq!(text(msg(false, "", "[0, 2]")), Some((Some((0, 2)), String::new())), "a revision to nothing clears");
        let meta: RealtimeMessage = serde_json::from_str(r#"{ "type": "Metadata" }"#).unwrap();
        assert!(nova_interim(&meta).is_none());
    }

    /// Deepgram's documented case: an interim over 0-4.3 s, then a final
    /// over 0-3.26 s. The uncovered words stay greyed.
    #[test]
    fn a_nova_final_keeps_the_interim_words_it_did_not_cover() {
        let w = |word: &str, start: f64, end: f64| DeepgramWord {
            word: word.to_lowercase(),
            punctuated_word: Some(word.to_string()),
            start,
            end,
            confidence: None,
            speaker: None,
        };
        let interim = vec![w("Another", 0.1, 0.5), w("big", 0.6, 3.2), w("problem", 3.27, 3.8), w("in", 3.9, 4.3)];
        assert_eq!(words_line(&interim_after(&interim, 3.26)), "problem in");
        assert_eq!(words_line(&interim_after(&interim, 4.3)), "", "a final over all of it clears");
        // A word the final holds stays out even if the interim timed it a
        // few ms earlier or later than the final's end.
        // The final's "go" ends at 3.26; the interim timed it 3.25-3.262.
        let edge = vec![w("go", 3.25, 3.262), w("now", 3.3, 3.6)];
        assert_eq!(words_line(&interim_after(&edge, 3.26)), "now");
        assert_eq!(words_line(&interim_after(&edge, 3.27)), "now");
        assert!(interim_after(&[], 1.0).is_empty());
    }

    #[test]
    fn greyed_mic_bleed_is_hidden_and_real_mic_words_are_not() {
        let mut f = InterimBleedFilter::default();
        let clear = |speaker| RealtimeOutput::Interim { speaker, text: String::new() };
        let shown = |speaker, text: &str| RealtimeOutput::Interim { speaker, text: text.to_string() };
        let far = Some(SPEAKER_OUT_LABEL);
        let mic = Some(MIC_IN_LABEL);
        assert_eq!(f.filter(far, "let us review the quarterly".into(), 0.0), shown(far, "let us review the quarterly"));
        // The far side's words through the mic, a beat later: hidden.
        assert_eq!(f.filter(mic, "review the quarterly".into(), 400.0), clear(mic));
        // The user's own words, said over the far side: shown.
        assert_eq!(f.filter(mic, "I am taking notes now".into(), 600.0), shown(mic, "I am taking notes now"));
        // Too short to judge: shown even though "the" was said.
        assert_eq!(f.filter(mic, "the review".into(), 700.0), shown(mic, "the review"));
        // The far side resending its utterance does not make its first words new.
        let mut g = InterimBleedFilter::default();
        g.filter(far, "approve the budget".into(), 0.0);
        g.filter(far, "approve the budget and then the hiring plan".into(), INTERIM_BLEED_WINDOW_MS - 100.0);
        assert_eq!(
            g.filter(far, "approve the budget and then the hiring plan for next year".into(), INTERIM_BLEED_WINDOW_MS + 500.0),
            shown(far, "approve the budget and then the hiring plan for next year")
        );
        assert_eq!(g.filter(mic, "approve the budget".into(), INTERIM_BLEED_WINDOW_MS + 600.0), shown(mic, "approve the budget"));
        // Old far-side words no longer count.
        assert_eq!(f.filter(mic, "review the quarterly".into(), 400.0 + INTERIM_BLEED_WINDOW_MS + 1.0), shown(mic, "review the quarterly"));
    }

    #[test]
    fn flux_reduction_only_on_end_of_turn() {
        let mk = |event: &str, transcript: &str| FluxTurnInfo {
            msg_type: Some("TurnInfo".to_string()),
            event: Some(event.to_string()),
            transcript: transcript.to_string(),
            audio_window_start: 0.0,
            audio_window_end: 8.32,
            words: vec![DeepgramWord {
                word: "Hello".to_string(),
                punctuated_word: None,
                start: 0.0,
                end: 0.48,
                confidence: Some(1.0),
                speaker: None,
            }],
            description: None,
            message: None,
        };
        assert!(flux_turn_to_final(&mk("Update", "Hello")).is_none());
        assert!(flux_turn_to_final(&mk("StartOfTurn", "Hello")).is_none());
        assert!(flux_turn_to_final(&mk("EndOfTurn", "   ")).is_none());
        let seg = flux_turn_to_final(&mk("EndOfTurn", "Hello. This is a test.")).unwrap();
        assert_eq!(seg.transcript, "Hello. This is a test.");
        // Timed by the words' span, not the 0-8.32 s window (see
        // a_flux_turn_is_timed_by_its_words_not_its_window).
        assert_eq!(seg.start_ms, 0.0);
        assert_eq!(seg.end_ms, 480.0);
    }

    #[test]
    fn parses_real_flux_turninfo_shape() {
        // Shape captured live from wss://api.deepgram.com/v2/listen (flux-general-multi).
        let json = r#"{
            "type": "TurnInfo", "request_id": "abc", "event": "EndOfTurn",
            "turn_index": 0, "audio_window_start": 0.0, "audio_window_end": 8.32,
            "transcript": "Hello. This is a flux test.",
            "words": [ { "word": "Hello.", "confidence": 1.0, "start": 0.0, "end": 0.48, "language": "en" } ],
            "languages": ["en"], "languages_hinted": ["en"],
            "end_of_turn_confidence": 0.83, "sequence_id": 12
        }"#;
        let m: FluxTurnInfo = serde_json::from_str(json).unwrap();
        assert_eq!(m.msg_type.as_deref(), Some("TurnInfo"));
        assert_eq!(m.event.as_deref(), Some("EndOfTurn"));
        let seg = flux_turn_to_final(&m).unwrap();
        assert_eq!(seg.transcript, "Hello. This is a flux test.");
    }

    #[test]
    fn is_flux_model_detects_flux() {
        assert!(is_flux_model("flux-general-en"));
        assert!(is_flux_model("flux-general-multi"));
        assert!(!is_flux_model("nova-3"));
        assert!(!is_flux_model("nova-2"));
    }

    #[test]
    fn flux_url_uses_v2_and_omits_nova_knobs() {
        let opts = DeepgramOptions {
            model: "flux-general-multi".to_string(),
            keyterms: vec!["Meetily".to_string()],
            diarize: true, // ignored by Flux
            mip_opt_out: false,
            language: None,
        };
        let url = build_flux_url(FLUX_WS_URL, &opts, Some("en".to_string()));
        assert!(url.starts_with("wss://api.deepgram.com/v2/listen?"));
        assert!(url.contains("model=flux-general-multi"));
        assert!(url.contains("language_hint=en")); // multi model biases via hint
        assert!(url.contains("keyterm=Meetily")); // Flux uses keyterm, not keywords
        assert!(!url.contains("smart_format"));
        assert!(!url.contains("diarize"));
        assert!(!url.contains("endpointing"));
        assert!(!url.contains("language=")); // no language tag; model selects language
    }

    #[test]
    fn a_replayed_line_keeps_only_what_its_channel_did_not_show() {
        let skip = ShownUntil { mic_in_ms: 10_000.0, speaker_out_ms: 4_000.0, unlabelled_ms: 0.0 };
        let text = |s: Option<LabeledSegment>| s.map(|s| s.text);
        // Live case: the replayed line ends a little after the cutoff.
        assert_eq!(text(skip.unshown(timed_seg("let me put together a quote", 7_000.0, 10_300.0, MIC_IN_LABEL))), None);
        // A replayed line that runs on past the cutoff keeps its new words.
        assert_eq!(
            text(skip.unshown(timed_seg("we said this then something new", 8_000.0, 12_000.0, MIC_IN_LABEL))),
            Some("something new".to_string())
        );
        // The far side's open turn (after its own cutoff) is kept whole.
        assert_eq!(
            text(skip.unshown(timed_seg("one more thing", 5_000.0, 6_000.0, SPEAKER_OUT_LABEL))),
            Some("one more thing".to_string())
        );
        // Untimed lines: kept when their midpoint is at or after the cutoff.
        assert_eq!(text(skip.unshown(seg("old", 8_000.0, 9_500.0, Some(MIC_IN_LABEL)))), None);
        assert!(skip.unshown(seg("new", 9_800.0, 11_000.0, Some(MIC_IN_LABEL))).is_some());
        assert!(
            skip.unshown(seg("midpoint exactly at the cutoff", 9_900.0, 10_100.0, Some(MIC_IN_LABEL))).is_some(),
            "cycle 2 finding 1: a midpoint at the cutoff is new speech"
        );
        assert!(ShownUntil::default().unshown(seg("x", 0.0, 0.0, None)).is_some(), "nothing shown: nothing skipped");
    }

    /// Cycle 2 finding 2: a diarized replayed line cut at the cutoff keeps its
    /// speaker numbering, clipped to the words it still shows.
    #[test]
    fn a_cut_replayed_line_keeps_its_diarized_runs() {
        let skip = ShownUntil { speaker_out_ms: 10_000.0, ..ShownUntil::default() };
        let mut line = timed_seg("we said this then you said that", 8_000.0, 15_000.0, SPEAKER_OUT_LABEL);
        let turn = |text: &str, start_ms: f64, end_ms: f64, speaker: i64, first_src: usize, last_src: usize| SpeakerTurn {
            text: text.into(),
            start_ms,
            end_ms,
            confidence: 0.9,
            speaker,
            first_src,
            last_src,
        };
        line.turns =
            vec![turn("we said this then", 8_000.0, 11_999.0, 0, 0, 3), turn("you said that", 12_000.0, 15_000.0, 1, 4, 6)];
        let kept = skip.unshown(line).expect("new words remain");
        assert_eq!(kept.text, "then you said that");
        let runs: Vec<(String, i64)> = kept.turns.iter().map(|t| (t.text.clone(), t.speaker)).collect();
        assert_eq!(runs, vec![("then".to_string(), 0), ("you said that".to_string(), 1)]);
        assert_eq!(kept.turns[0].start_ms, kept.start_ms, "the first run starts at the first kept word");
        let rows: Vec<Option<String>> = transcript_lines(kept).into_iter().map(|l| l.speaker).collect();
        assert_eq!(rows, vec![Some("Speaker-Out 1".to_string()), Some("Speaker-Out 2".to_string())]);
    }

    /// Cycle 2 round 1: Deepgram's diarized runs touch (one ends where the
    /// next begins), so a word on the boundary must go to one run only, and
    /// a clipped run reports the confidence of the words it still holds.
    #[test]
    fn a_word_on_a_run_boundary_goes_to_one_run_and_confidence_follows_the_words() {
        let skip = ShownUntil { speaker_out_ms: 500.0, ..ShownUntil::default() };
        let mut line = timed_seg("old kept new", 0.0, 1_500.0, SPEAKER_OUT_LABEL);
        // Words start at 0, 500, 1000 ms; runs touch at 1000 ms.
        for (w, c) in line.words.iter_mut().zip([0.1, 0.9, 0.7]) {
            w.confidence = Some(c);
        }
        let turn = |text: &str, start_ms: f64, end_ms: f64, speaker: i64, first_src: usize, last_src: usize| SpeakerTurn {
            text: text.into(),
            start_ms,
            end_ms,
            confidence: 0.5,
            speaker,
            first_src,
            last_src,
        };
        line.turns = vec![turn("old kept", 0.0, 1_000.0, 0, 0, 1), turn("new", 1_000.0, 1_500.0, 1, 2, 2)];
        // Cycle 2 round 2: two speakers' words starting together stay apart.
        let mut same_start = line.clone();
        same_start.words[1].start_ms = 1_000.0; // "kept" (speaker 0) and "new" (speaker 1) both at 1000 ms
        let runs: Vec<(String, i64)> = with_words(&same_start, |w| w.start_ms >= 1_000.0)
            .unwrap()
            .turns
            .iter()
            .map(|t| (t.text.clone(), t.speaker))
            .collect();
        assert_eq!(runs, vec![("kept".to_string(), 0), ("new".to_string(), 1)]);
        // Cut at 500 + jitter: "kept" (starts at 500) is not after it, so only
        // "new" (1000) stays, on the boundary of both runs.
        let kept = skip.unshown(line.clone()).unwrap();
        let runs: Vec<(String, i64)> = kept.turns.iter().map(|t| (t.text.clone(), t.speaker)).collect();
        assert_eq!(runs, vec![("new".to_string(), 1)], "the boundary word is in run 1 only");
        // Keep "kept" and "new": run 0 is clipped to "kept" with its confidence.
        let both = with_words(&line, |w| w.start_ms >= 500.0).unwrap();
        let runs: Vec<(String, f32)> = both.turns.iter().map(|t| (t.text.clone(), t.confidence)).collect();
        assert_eq!(runs, vec![("kept".to_string(), 0.9), ("new".to_string(), 0.7)]);
        assert!((both.confidence - 0.8).abs() < 1e-6, "line confidence from its kept words: {}", both.confidence);
    }

    #[test]
    fn a_flux_turn_is_timed_by_its_words_not_its_window() {
        let m: FluxTurnInfo = serde_json::from_value(serde_json::json!({
            "type": "TurnInfo", "event": "EndOfTurn", "transcript": "Yes. We handle",
            "audio_window_start": 0.0, "audio_window_end": 14.2,
            "words": [
                {"word": "Yes.", "confidence": 1.0, "start": 9.92, "end": 10.32},
                {"word": "We", "confidence": 1.0, "start": 10.32, "end": 10.48},
                {"word": "handle", "confidence": 1.0, "start": 10.48, "end": 10.8}
            ]
        }))
        .unwrap();
        let seg = flux_turn_to_final(&m).unwrap();
        assert_eq!((seg.start_ms, seg.end_ms), (9920.0, 10800.0));
        let untimed: FluxTurnInfo = serde_json::from_value(serde_json::json!({
            "type": "TurnInfo", "event": "EndOfTurn", "transcript": "hi", "audio_window_start": 1.0, "audio_window_end": 2.0
        }))
        .unwrap();
        assert_eq!(flux_turn_to_final(&untimed).map(|s| (s.start_ms, s.end_ms)), Some((1000.0, 2000.0)));
    }

    #[test]
    fn flux_url_carries_the_mip_opt_out() {
        let opts = DeepgramOptions { model: "flux-general-en".to_string(), ..DeepgramOptions::default() };
        assert!(opts.mip_opt_out, "privacy-first default");
        assert!(build_flux_url(FLUX_WS_URL, &opts, None).contains("mip_opt_out=true"));
        let opted_in = DeepgramOptions { mip_opt_out: false, ..opts };
        assert!(!build_flux_url(FLUX_WS_URL, &opted_in, None).contains("mip_opt_out"));
    }

    #[test]
    fn sidecar_lanes_are_named_by_side() {
        let stereo = plan_lanes(true, 2);
        assert_eq!(lane_side(AudioLayout::MicSystemStereo, stereo[0]), "me");
        assert_eq!(lane_side(AudioLayout::MicSystemStereo, stereo[1]), "them");
        assert_eq!(lane_side(AudioLayout::MicOnly, plan_lanes(true, 1)[0]), "me");
        assert_eq!(lane_side(AudioLayout::Mono, plan_lanes(true, 1)[0]), "unknown");
        assert!(!RealtimeEndpoints::deepgram().tag_lane_side, "Deepgram rejects unknown query params");
    }

    /// WORK-559: the real sidecar (fake Flux, no network) behind the real
    /// realtime core. Both lanes are labelled from the sidecar's TurnInfo, and
    /// killing the sidecar mid-recording hands the rest of the audio back for
    /// the Direct fallback instead of silently ending transcription.
    #[tokio::test]
    #[ignore = "starts the real sidecar process: needs uv and `uv sync` in pipecat-sidecar"]
    async fn pipecat_sidecar_feeds_the_core_and_a_crash_falls_back() {
        use crate::audio::pipecat::{relay, tests::{dev_command, TEST_STARTUP_TIMEOUT}, Sidecar};
        let sidecar = Sidecar::spawn(dev_command(), "dg-test", TEST_STARTUP_TIMEOUT).await.expect("sidecar starts");
        let pid = sidecar.pid().unwrap();
        let options = DeepgramOptions { model: "flux-general-en".to_string(), ..DeepgramOptions::default() };
        let (outer_tx, outer_rx) = mpsc::unbounded_channel::<AudioChunk>();
        let (inner_tx, inner_rx) = mpsc::unbounded_channel::<AudioChunk>();
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<LabeledSegment>();
        let interims: Arc<std::sync::Mutex<Vec<(Option<&'static str>, String)>>> = Arc::default();
        let interim_sink = interims.clone();
        let core = run_realtime_core(sidecar.endpoints(), sidecar.token.clone(), options, None, inner_rx, move |out: RealtimeOutput| {
            match out {
                RealtimeOutput::Show { seg, .. } => {
                    let _ = seen_tx.send(seg);
                }
                RealtimeOutput::Interim { speaker, text } => interim_sink.lock().unwrap().push((speaker, text)),
                RealtimeOutput::Retract { .. } => {}
            }
        });
        // 0.1 s stereo chunks; the fake Flux commits one turn per 0.1 s per lane.
        let send = |i: u64| {
            let frames = 1600;
            let data: Vec<f32> = (0..frames * 2).map(|n| if n % 2 == 0 { 0.1 } else { -0.1 }).collect();
            outer_tx
                .send(AudioChunk {
                    data,
                    sample_rate: 16_000,
                    timestamp: i as f64 * 0.1,
                    chunk_id: i,
                    device_type: crate::audio::recording_state::DeviceType::Microphone,
                    layout: AudioLayout::MicSystemStereo,
                })
                .is_ok()
        };
        let feeder = async {
            let mut labels = std::collections::HashSet::new();
            for i in 0..10 {
                assert!(send(i));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            while labels.len() < 2 {
                let seg = tokio::time::timeout(Duration::from_secs(10), seen_rx.recv()).await.expect("a turn").expect("open");
                assert!(seg.text.starts_with("turn "), "{}", seg.text);
                assert!(!seg.words.is_empty(), "word timings survive the sidecar");
                labels.insert(seg.speaker);
            }
            assert_eq!(labels, [Some(MIC_IN_LABEL), Some(SPEAKER_OUT_LABEL)].into_iter().collect());
            // WORK-580: each side's Flux Update reached the core as greyed
            // interim text, and each committed turn cleared it.
            {
                let interims = interims.lock().unwrap();
                for side in [MIC_IN_LABEL, SPEAKER_OUT_LABEL] {
                    assert!(interims.iter().any(|(s, t)| *s == Some(side) && t == "turn"), "{side}: {interims:?}");
                    assert!(interims.iter().any(|(s, t)| *s == Some(side) && t.is_empty()), "{side} never cleared: {interims:?}");
                }
            }
            // Crash the sidecar mid-recording.
            assert!(std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap().success());
            for i in 10..60 {
                if !send(i) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let ((rest, result), ()) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(relay(outer_rx, inner_tx, core, |_: &AudioChunk| {}), feeder)
        })
        .await
        .expect("the crash is noticed, not waited on forever");
        assert!(result.is_err(), "a crashed sidecar is an error, not a quiet end: {result:?}");
        let mut rest = rest.expect("the recording continues on Direct");
        let mut leftover = 0;
        while rest.try_recv().is_ok() {
            leftover += 1;
        }
        assert!(leftover > 0, "audio after the crash is handed to the fallback");
        sidecar.shutdown().await;
    }

    #[test]
    fn flux_en_url_omits_language_hint() {
        let opts = DeepgramOptions {
            model: "flux-general-en".to_string(),
            keyterms: vec![],
            diarize: false,
            mip_opt_out: false,
            language: None,
        };
        let url = build_flux_url(FLUX_WS_URL, &opts, Some("en".to_string()));
        assert!(!url.contains("language_hint"));
    }

    #[test]
    fn prerecorded_falls_back_to_nova_for_flux() {
        // Flux is realtime-only; file transcription must use a nova model.
        let flux = DeepgramClient::new(
            "k".to_string(),
            DeepgramOptions {
                model: "flux-general-en".to_string(),
                keyterms: vec!["Meetily".to_string()],
                diarize: true,
                mip_opt_out: false,
                language: None,
            },
        );
        let q = flux.prerecorded_query(None);
        let model = &q.iter().find(|(k, _)| k == "model").unwrap().1;
        assert_eq!(model, crate::config::DEFAULT_DEEPGRAM_ASYNC_MODEL);
        assert!(!is_flux_model(model));
        // keyterm still routes correctly for the nova fallback model.
        assert!(q.iter().any(|(k, _)| k == "keyterm"));
    }

    // ------------------------------------------------------------------
    // Multichannel realtime: labels, lanes, channel parsing, time mapping
    // ------------------------------------------------------------------

    use super::super::recording_state::DeviceType;

    #[test]
    fn channel_labels_follow_layout() {
        use AudioLayout::*;
        assert_eq!(channel_label(MicSystemStereo, 0), Some("Mic-In"));
        assert_eq!(channel_label(MicSystemStereo, 1), Some("Speaker-Out"));
        // Mic-only recording: the one channel is the mic.
        assert_eq!(channel_label(MicOnly, 0), Some("Mic-In"));
        // A mixed or raw mono stream has no single source.
        assert_eq!(channel_label(Mono, 0), None);
        assert_eq!(channel_label(MicSystemStereo, 2), None);
    }

    /// Finding 7: the frontend keeps its own copy of the labels for styling and
    /// export. Read it and require exactly these two values.
    #[test]
    fn ts_speaker_labels_match_rust_constants() {
        let ts = include_str!("../../../src/lib/transcript-speakers.ts");
        let labels: Vec<(&str, &str)> = ts
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("export const ")?;
                let (name, value) = rest.split_once(" = ")?;
                if !name.ends_with("_LABEL") {
                    return None;
                }
                Some((name, value.trim_end_matches(';').trim_matches('\'')))
            })
            .collect();
        assert_eq!(
            labels,
            vec![("MIC_IN_LABEL", MIC_IN_LABEL), ("SPEAKER_OUT_LABEL", SPEAKER_OUT_LABEL)]
        );
    }

    /// The frontend must listen on the exact event the transcriber emits, or
    /// retracted bleed silently stays on screen and in the saved transcript.
    #[test]
    fn ts_listens_for_transcript_retract() {
        let ts = include_str!("../../../src/lib/transcript-retract.ts");
        let value = ts
            .lines()
            .find_map(|l| l.trim().strip_prefix("export const TRANSCRIPT_RETRACT_EVENT = "))
            .expect("TRANSCRIPT_RETRACT_EVENT not found in transcript-retract.ts");
        assert_eq!(value.trim_end_matches(';').trim_matches('\''), TRANSCRIPT_RETRACT_EVENT);
        let service = include_str!("../../../src/services/transcriptService.ts");
        assert!(service.contains("listen<TranscriptRetract>(TRANSCRIPT_RETRACT_EVENT"));
    }

    /// Same contract for the greyed live line: a renamed event would leave the
    /// interim words silently invisible.
    #[test]
    fn ts_listens_for_transcript_interim() {
        let ts = include_str!("../../../src/lib/transcript-interim.ts");
        let value = ts
            .lines()
            .find_map(|l| l.trim().strip_prefix("export const TRANSCRIPT_INTERIM_EVENT = "))
            .expect("TRANSCRIPT_INTERIM_EVENT not found in transcript-interim.ts");
        assert_eq!(value.trim_end_matches(';').trim_matches('\''), TRANSCRIPT_INTERIM_EVENT);
        let service = include_str!("../../../src/services/transcriptService.ts");
        assert!(service.contains("listen<TranscriptInterim>(TRANSCRIPT_INTERIM_EVENT"));
    }

    /// The frontend skips its local-model check for these providers, so each
    /// must be one the Rust start path validates itself. Reads the TS source,
    /// so adding a provider there without wiring it here fails.
    #[test]
    fn ts_cloud_providers_are_ones_recording_validates() {
        let ts = include_str!("../../../src/lib/transcription-model-readiness.ts");
        let line = ts
            .lines()
            .find(|l| l.contains("const CLOUD_PROVIDERS = new Set(["))
            .expect("CLOUD_PROVIDERS declaration not found in transcription-model-readiness.ts");
        let inner = line.split_once("([").unwrap().1.split_once("])").unwrap().0;
        let providers: Vec<&str> = inner.split(',').map(|p| p.trim().trim_matches('\'')).collect();
        assert_eq!(providers, vec![PROVIDER]);
    }

    #[test]
    fn stereo_realtime_url_requests_multichannel() {
        let url = build_realtime_url(LISTEN_WS_URL, &DeepgramOptions::default(), None, 2);
        assert!(url.contains("channels=2"));
        assert!(url.contains("multichannel=true"));
    }

    #[test]
    fn nova_uses_one_socket_and_flux_one_socket_per_channel() {
        assert_eq!(plan_lanes(false, 2), vec![Lane { first_channel: 0, channels: 2 }]);
        assert_eq!(
            plan_lanes(true, 2),
            vec![
                Lane { first_channel: 0, channels: 1 },
                Lane { first_channel: 1, channels: 1 }
            ]
        );
        assert_eq!(plan_lanes(false, 1), vec![Lane { first_channel: 0, channels: 1 }]);
        assert_eq!(plan_lanes(true, 1), vec![Lane { first_channel: 0, channels: 1 }]);
    }

    #[test]
    fn nova_final_carries_channel_index() {
        let json = r#"{
            "type": "Results", "channel_index": [1, 2], "start": 2.0, "duration": 1.0,
            "is_final": true,
            "channel": { "alternatives": [ { "transcript": "Life moves pretty fast.", "words": [] } ] }
        }"#;
        let fin = nova_final(serde_json::from_str(json).unwrap()).unwrap();
        assert_eq!(fin.channel_index, Some((1, 2)));
        assert_eq!(fin.seg.unwrap().transcript, "Life moves pretty fast.");
    }

    #[test]
    fn diarized_final_stays_whole_and_carries_speaker_turns() {
        let json = r#"{
            "type": "Results", "channel_index": [1, 2], "start": 2.0, "duration": 2.0,
            "is_final": true,
            "channel": { "alternatives": [ { "transcript": "Hi there. Hello.", "confidence": 0.95, "words": [
                { "word": "hi", "punctuated_word": "Hi", "start": 2.0, "end": 2.3, "confidence": 0.8, "speaker": 0 },
                { "word": "there", "punctuated_word": "there.", "start": 2.3, "end": 2.7, "confidence": 1.0, "speaker": 0 },
                { "word": "hello", "punctuated_word": "Hello.", "start": 3.1, "end": 3.6, "confidence": 0.9, "speaker": 1 }
            ] } ] }
        }"#;
        let fin = nova_final(serde_json::from_str(json).unwrap()).unwrap();
        let seg = fin.seg.unwrap();
        // Whole final kept for bleed suppression.
        assert_eq!(seg.transcript, "Hi there. Hello.");
        assert_eq!((seg.start_ms, seg.end_ms), (2000.0, 4000.0));
        assert_eq!(seg.turns.len(), 2);
        assert_eq!(seg.turns[0].text, "Hi there.");
        assert_eq!(seg.turns[0].speaker, 0);
        assert_eq!((seg.turns[0].start_ms, seg.turns[0].end_ms), (2000.0, 2700.0));
        assert!((seg.turns[0].confidence - 0.9).abs() < 1e-6);
        assert_eq!(seg.turns[1].text, "Hello.");
        assert_eq!(seg.turns[1].speaker, 1);
        assert_eq!((seg.turns[1].start_ms, seg.turns[1].end_ms), (3100.0, 3600.0));
    }

    #[test]
    fn undiarized_final_has_no_turns() {
        let json = r#"{ "type": "Results", "channel_index": [0, 2], "start": 0.0, "duration": 1.0,
            "is_final": true, "channel": { "alternatives": [ { "transcript": "Just me.", "words": [
                { "word": "just", "punctuated_word": "Just", "start": 0.1, "end": 0.3 },
                { "word": "me", "punctuated_word": "me.", "start": 0.3, "end": 0.5 }
            ] } ] } }"#;
        let seg = nova_result_to_final(serde_json::from_str(json).unwrap()).unwrap();
        assert_eq!(seg.transcript, "Just me.");
        assert!(seg.turns.is_empty());
        assert_eq!((seg.start_ms, seg.end_ms), (0.0, 1000.0));
    }

    #[test]
    fn word_missing_speaker_id_stays_in_current_turn() {
        let words = vec![
            word("one", 0.0, 0.2, Some(1)),
            word("two", 0.2, 0.4, None),
            word("three", 0.4, 0.6, Some(1)),
            word("four", 0.6, 0.8, Some(0)),
        ];
        let turns = diarized_turns(&words);
        let summary: Vec<(&str, i64)> = turns.iter().map(|t| (t.text.as_str(), t.speaker)).collect();
        assert_eq!(summary, vec![("one two three", 1), ("four", 0)]);
    }

    #[test]
    fn empty_punctuated_word_falls_back_to_raw_word() {
        let mut w = word("okay", 0.0, 0.2, Some(0));
        w.punctuated_word = Some(String::new());
        assert_eq!(word_display(&w), "okay");
        assert_eq!(diarized_turns(&[w])[0].text, "okay");
    }

    fn turn(text: &str, start_ms: f64, end_ms: f64, speaker: i64) -> SpeakerTurn {
        SpeakerTurn { text: text.to_string(), start_ms, end_ms, confidence: 0.8, speaker, first_src: 0, last_src: 0 }
    }

    #[test]
    fn transcript_lines_split_multi_voice_finals_only() {
        let mut multi = seg("Hi there. Hello.", 2000.0, 4000.0, Some(SPEAKER_OUT_LABEL));
        multi.turns = vec![turn("Hi there.", 2000.0, 2700.0, 0), turn("Hello.", 3100.0, 3600.0, 1)];
        let lines = transcript_lines(multi);
        let summary: Vec<(&str, Option<&str>, f64)> =
            lines.iter().map(|l| (l.text.as_str(), l.speaker.as_deref(), l.start_ms)).collect();
        assert_eq!(
            summary,
            vec![("Hi there.", Some("Speaker-Out 1"), 2000.0), ("Hello.", Some("Speaker-Out 2"), 3100.0)]
        );

        // One voice: the whole final, window and confidence, numbered.
        let mut single = seg("Just me.", 0.0, 1000.0, Some(MIC_IN_LABEL));
        single.turns = vec![turn("Just me.", 100.0, 500.0, 0)];
        let lines = transcript_lines(single);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].speaker.as_deref(), Some("Mic-In 1"));
        assert_eq!((lines[0].start_ms, lines[0].end_ms, lines[0].confidence), (0.0, 1000.0, 0.9));

        // Diarization off: unchanged label.
        let lines = transcript_lines(seg("Plain.", 0.0, 500.0, Some(SPEAKER_OUT_LABEL)));
        assert_eq!(lines[0].speaker.as_deref(), Some("Speaker-Out"));
    }

    /// Review finding: splitting before suppression let a mic copy of a
    /// two-voice Speaker-Out final through, since each 3-word piece fell under
    /// the overlap and containment thresholds. Suppression sees whole finals.
    #[test]
    fn bleed_of_a_two_voice_final_is_still_dropped() {
        let mut s = BleedSuppressor::default();
        let mut far = seg("Alpha beta gamma. Delta epsilon zeta.", 1000.0, 4000.0, Some(SPEAKER_OUT_LABEL));
        far.turns = vec![
            turn("Alpha beta gamma.", 1000.0, 2400.0, 0),
            turn("Delta epsilon zeta.", 2600.0, 4000.0, 1),
        ];
        let out = s.push(far.clone(), 0.0);
        assert_eq!(out, vec![RealtimeOutput::Show { key: 0, seg: far }]);
        let mic = seg("Alpha beta gamma. Delta epsilon zeta.", 1050.0, 4000.0, Some(MIC_IN_LABEL));
        assert!(s.push(mic, 100.0).is_empty());
    }

    #[test]
    fn segment_label_numbers_diarized_voices_from_one() {
        assert_eq!(segment_label(Some(SPEAKER_OUT_LABEL), None).as_deref(), Some("Speaker-Out"));
        assert_eq!(segment_label(Some(SPEAKER_OUT_LABEL), Some(1)).as_deref(), Some("Speaker-Out 2"));
        assert_eq!(segment_label(Some(MIC_IN_LABEL), Some(0)).as_deref(), Some("Mic-In 1"));
        assert_eq!(segment_label(None, Some(0)).as_deref(), Some("Speaker 1"));
        assert_eq!(segment_label(None, None), None);
    }

    #[test]
    fn diarization_is_off_by_default() {
        let url = build_realtime_url(LISTEN_WS_URL, &DeepgramOptions::default(), None, 2);
        assert!(!url.contains("diarize"));
        assert!(url.contains("multichannel=true"));
    }

    /// Finding 6: on a multichannel socket a missing or invalid channel_index
    /// is unlabelled, never defaulted to channel 0 (= Mic-In).
    #[test]
    fn stereo_result_without_valid_channel_index_is_unlabelled() {
        let stereo = Lane { first_channel: 0, channels: 2 };
        assert_eq!(resolve_channel(stereo, Some((1, 2))), Some(1));
        assert_eq!(resolve_channel(stereo, None), None);
        assert_eq!(resolve_channel(stereo, Some((2, 2))), None);
        assert_eq!(resolve_channel(stereo, Some((0, 1))), None);
        // A mono socket is unambiguous whatever it reports.
        assert_eq!(resolve_channel(Lane { first_channel: 1, channels: 1 }, None), Some(1));
        assert_eq!(parse_channel_index(&serde_json::json!("oops")), None);
        assert_eq!(parse_channel_index(&serde_json::json!([1.5, 2])), None);
        assert_eq!(parse_channel_index(&serde_json::json!([1, 2])), Some((1, 2)));
    }

    fn chunk(data: Vec<f32>, sample_rate: u32, layout: AudioLayout) -> AudioChunk {
        AudioChunk {
            data,
            sample_rate,
            timestamp: 0.0,
            chunk_id: 0,
            device_type: DeviceType::Microphone,
            layout,
        }
    }

    #[test]
    fn lane_samples_keeps_channels_apart_at_native_rate() {
        // 16 kHz input needs no resampling, so samples must pass through exactly.
        let stereo = chunk(vec![0.1, -0.5, 0.2, -0.6, 0.3, -0.7], 16_000, AudioLayout::MicSystemStereo);
        assert_eq!(
            lane_samples(&stereo, Lane { first_channel: 0, channels: 2 }).unwrap(),
            vec![0.1, -0.5, 0.2, -0.6, 0.3, -0.7]
        );
        assert_eq!(lane_samples(&stereo, Lane { first_channel: 0, channels: 1 }).unwrap(), vec![0.1, 0.2, 0.3]);
        assert_eq!(lane_samples(&stereo, Lane { first_channel: 1, channels: 1 }).unwrap(), vec![-0.5, -0.6, -0.7]);
    }

    #[test]
    fn lane_samples_resamples_each_channel_separately() {
        // 48 kHz: left is a constant +0.5 DC level, right is silence. If the
        // interleaved buffer were resampled as one signal the right channel
        // would pick up energy from the left.
        let frames = 4800; // 100 ms
        let mut data = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            data.push(0.5);
            data.push(0.0);
        }
        let out = lane_samples(&chunk(data, 48_000, AudioLayout::MicSystemStereo), Lane { first_channel: 0, channels: 2 }).unwrap();
        let split = deinterleave(&out, 2);
        assert_eq!(split[0].len(), split[1].len());
        let n = split[0].len();
        assert!((1500..=1700).contains(&n), "expected ~1600 frames at 16 kHz, got {}", n);
        assert!(split[1].iter().all(|s| s.abs() < 1e-6), "right channel must stay silent");
        let mid = &split[0][n / 4..3 * n / 4];
        assert!(mid.iter().all(|s| (s - 0.5).abs() < 0.05), "left channel must keep its level");
    }

    #[test]
    fn mono_chunk_passes_through_unchanged() {
        let mono = chunk(vec![0.1, 0.2, 0.3], 16_000, AudioLayout::MicOnly);
        assert_eq!(lane_samples(&mono, Lane { first_channel: 0, channels: 1 }).unwrap(), vec![0.1, 0.2, 0.3]);
    }

    /// Finding 8: a channel that fails to resample drops the chunk instead of
    /// sending 48 kHz samples labelled as 16 kHz next to a resampled channel.
    #[test]
    fn resample_failure_on_one_channel_drops_the_chunk() {
        let stereo = chunk(vec![0.1, 0.2, 0.3, 0.4], 48_000, AudioLayout::MicSystemStereo);
        let fail_right = |samples: &[f32], _: u32, _: u32| -> Result<Vec<f32>> {
            if samples[0] == 0.2 {
                Err(anyhow!("resampler exploded"))
            } else {
                Ok(samples.to_vec())
            }
        };
        assert_eq!(lane_samples_with(&stereo, Lane { first_channel: 0, channels: 2 }, fail_right), None);
        // The mono lane for the healthy channel is unaffected.
        assert_eq!(
            lane_samples_with(&stereo, Lane { first_channel: 0, channels: 1 }, fail_right),
            Some(vec![0.1, 0.3])
        );
    }

    /// Finding 8: channels that resample to different lengths are padded when
    /// the skew is rounding-sized, dropped when it is not.
    #[test]
    fn unequal_resampled_lengths_pad_small_skew_and_drop_large() {
        let stereo = chunk(vec![1.0; 200], 48_000, AudioLayout::MicSystemStereo);
        let lane = Lane { first_channel: 0, channels: 2 };
        let call = std::sync::atomic::AtomicUsize::new(0);
        let skewed = |short_by: usize| {
            call.store(0, Ordering::SeqCst);
            lane_samples_with(&stereo, lane, |_s: &[f32], _: u32, _: u32| {
                let n = if call.fetch_add(1, Ordering::SeqCst) == 0 { 30 } else { 30 - short_by };
                Ok(vec![1.0; n])
            })
        };
        let small = skewed(1).unwrap();
        assert_eq!(small.len(), 60, "1-sample skew is padded to equal frames");
        assert_eq!(small[59], 0.0, "the pad is silence on the short channel");
        assert_eq!(skewed(10), None, "a large skew drops the chunk");
    }

    #[test]
    fn stereo_chunk_duration_counts_frames_not_samples() {
        // 1600 stereo frames at 16 kHz = 100 ms, even though data holds 3200 samples.
        let stereo = chunk(vec![0.0; 3200], 16_000, AudioLayout::MicSystemStereo);
        assert_eq!(chunk_duration_ms(&stereo), 100.0);
        let mono = chunk(vec![0.0; 3200], 16_000, AudioLayout::MicOnly);
        assert_eq!(chunk_duration_ms(&mono), 200.0);
    }

    #[test]
    fn stereo_time_map_maps_stream_time_to_recording_time() {
        let entries = vec![
            AudioTimeMapEntry { stream_start_ms: 0.0, stream_end_ms: 100.0, original_start_ms: 5000.0, original_end_ms: 5100.0 },
            AudioTimeMapEntry { stream_start_ms: 100.0, stream_end_ms: 200.0, original_start_ms: 5100.0, original_end_ms: 5200.0 },
        ];
        assert_eq!(map_stream_ms_to_original(&entries, 150.0), 5150.0);
    }

    // ------------------------------------------------------------------
    // Bleed suppression
    // ------------------------------------------------------------------

    fn seg(text: &str, start_ms: f64, end_ms: f64, speaker: Option<&'static str>) -> LabeledSegment {
        LabeledSegment { text: text.to_string(), start_ms, end_ms, confidence: 0.9, speaker, turns: Vec::new(), words: Vec::new() }
    }

    fn shown(key: u64, seg: LabeledSegment) -> RealtimeOutput {
        RealtimeOutput::Show { key, seg }
    }

    /// What a transcript shows after these outputs (Show adds, Retract removes).
    fn visible(outputs: &[RealtimeOutput]) -> Vec<LabeledSegment> {
        let mut lines: Vec<(u64, LabeledSegment)> = Vec::new();
        for out in outputs {
            match out {
                RealtimeOutput::Show { key, seg } => lines.push((*key, seg.clone())),
                RealtimeOutput::Retract { key } => lines.retain(|(k, _)| k != key),
                RealtimeOutput::Interim { .. } => {}
            }
        }
        lines.into_iter().map(|(_, seg)| seg).collect()
    }

    /// The latency fix: a Mic-In final is shown the moment it arrives. The old
    /// watermark hold returned nothing here and released it seconds later.
    #[test]
    fn genuine_mic_speech_is_shown_immediately() {
        let mut s = BleedSuppressor::default();
        let mic = seg("Sounds good, let's ship it.", 1500.0, 2500.0, Some(MIC_IN_LABEL));
        assert_eq!(s.push(mic.clone(), 0.0), vec![shown(0, mic)]);
    }

    #[test]
    fn bleed_never_shown_when_speaker_copy_arrives_first() {
        let mut s = BleedSuppressor::default();
        let far = seg("Thanks for joining, Jake.", 1000.0, 1800.0, Some(SPEAKER_OUT_LABEL));
        assert_eq!(s.push(far.clone(), 0.0), vec![shown(0, far)]);
        assert!(s.push(seg("Thanks for joining, Jake.", 1050.0, 1800.0, Some(MIC_IN_LABEL)), 100.0).is_empty());
    }

    #[test]
    fn bleed_retracted_when_mic_copy_arrives_first() {
        let mut s = BleedSuppressor::default();
        let mic = seg("Bye bye.", 3000.0, 3500.0, Some(MIC_IN_LABEL));
        let far = seg("Bye bye.", 3000.0, 3600.0, Some(SPEAKER_OUT_LABEL));
        let mut outputs = s.push(mic.clone(), 0.0);
        assert_eq!(outputs, vec![shown(0, mic)]);
        let later = s.push(far.clone(), 500.0);
        assert_eq!(later, vec![shown(1, far.clone()), RealtimeOutput::Retract { key: 0 }]);
        outputs.extend(later);
        assert_eq!(visible(&outputs), vec![far]);
    }

    /// Finding 1, kept under the new design: the Speaker-Out final containing
    /// a mic copy can arrive well after it, and still takes it back.
    #[test]
    fn mic_copy_retracted_by_late_longer_speaker_final() {
        let mut s = BleedSuppressor::default();
        let mut outputs = s.push(seg("Thanks, Jake. See you then.", 10_000.0, 11_500.0, Some(MIC_IN_LABEL)), 0.0);
        outputs.extend(s.push(
            seg("Alright. Thanks, Jake. See you then, bye.", 9_000.0, 12_500.0, Some(SPEAKER_OUT_LABEL)),
            4_000.0,
        ));
        let lines = visible(&outputs);
        assert_eq!(lines.len(), 1, "only the Speaker-Out line should remain: {:?}", lines);
        assert_eq!(lines[0].speaker, Some(SPEAKER_OUT_LABEL));
    }

    #[test]
    fn unrelated_speaker_final_leaves_mic_line_alone() {
        let mut s = BleedSuppressor::default();
        let mic = seg("Different words entirely here.", 1000.0, 2000.0, Some(MIC_IN_LABEL));
        s.push(mic, 0.0);
        let out = s.push(seg("Life moves pretty fast.", 1000.0, 4000.0, Some(SPEAKER_OUT_LABEL)), 100.0);
        assert_eq!(out.len(), 1, "speaker line only, no retraction: {:?}", out);
    }

    /// Memory is bounded: past BLEED_MEMORY_MS a shown mic line is final.
    /// Literals, not the constant, so a changed window shows up here.
    #[test]
    fn mic_line_older_than_memory_is_never_retracted() {
        let mut s = BleedSuppressor::default();
        s.push(seg("Thanks for joining, Jake.", 1000.0, 1800.0, Some(MIC_IN_LABEL)), 0.0);
        let out = s.push(seg("Thanks for joining, Jake.", 1000.0, 1800.0, Some(SPEAKER_OUT_LABEL)), 30_001.0);
        assert_eq!(out.len(), 1, "no retraction after 30s: {:?}", out);
        let mut s = BleedSuppressor::default();
        s.push(seg("Thanks for joining, Jake.", 1000.0, 1800.0, Some(MIC_IN_LABEL)), 0.0);
        let out = s.push(seg("Thanks for joining, Jake.", 1000.0, 1800.0, Some(SPEAKER_OUT_LABEL)), 29_000.0);
        assert_eq!(out.len(), 2, "retracted within 30s: {:?}", out);
    }

    #[test]
    fn unlabeled_segments_pass_straight_through() {
        let mut s = BleedSuppressor::default();
        let mixed = seg("Mixed audio.", 0.0, 500.0, None);
        assert_eq!(s.push(mixed.clone(), 0.0), vec![shown(0, mixed)]);
    }

    #[test]
    fn partial_copy_inside_longer_speaker_segment_is_bleed() {
        let mic = seg("I can work with whomever, I just need to know", 1000.0, 3000.0, Some(MIC_IN_LABEL));
        let spk = seg(
            "Yeah. Like I said, I can work with whomever. I just need to know who.",
            900.0,
            4000.0,
            Some(SPEAKER_OUT_LABEL),
        );
        assert!(is_bleed(&mic, &spk));
    }

    #[test]
    fn same_words_far_apart_in_time_are_not_bleed() {
        let mic = seg("Thanks for joining, Jake.", 60_000.0, 61_000.0, Some(MIC_IN_LABEL));
        let spk = seg("Thanks for joining, Jake.", 1000.0, 2000.0, Some(SPEAKER_OUT_LABEL));
        assert!(!is_bleed(&mic, &spk));
    }

    #[test]
    fn short_word_inside_long_speaker_segment_is_not_bleed() {
        let mic = seg("Yeah.", 1000.0, 1300.0, Some(MIC_IN_LABEL));
        let spk = seg("Yeah, so the plan for the demo is next week.", 900.0, 4000.0, Some(SPEAKER_OUT_LABEL));
        assert!(!is_bleed(&mic, &spk));
    }

    /// Finding 3: a genuine short reply comes after the far side, not on top of it.
    #[test]
    fn short_reply_after_the_far_side_is_not_bleed() {
        let spk = seg("Okay.", 5_000.0, 5_400.0, Some(SPEAKER_OUT_LABEL));
        let mic = seg("Okay.", 5_800.0, 6_200.0, Some(MIC_IN_LABEL));
        assert!(!is_bleed(&mic, &spk));
    }

    /// Spread `text`'s words evenly over [start_ms, end_ms], like Deepgram's
    /// word timings.
    fn timed(text: &str, start_ms: f64, end_ms: f64) -> Vec<TimedWord> {
        let tokens: Vec<String> = text
            .split_whitespace()
            .flat_map(|w| word_tokens(w).into_iter().collect::<Vec<_>>())
            .collect();
        let step = (end_ms - start_ms) / tokens.len() as f64;
        tokens
            .into_iter()
            .enumerate()
            .map(|(i, token)| TimedWord {
                word: token.clone(),
                token,
                start_ms: start_ms + i as f64 * step,
                end_ms: start_ms + (i + 1) as f64 * step,
                src: i,
                confidence: None,
            })
            .collect()
    }

    // ---- bleed trimming (WORK-559: Flux merges own speech with bleed) ----

    fn texts(out: &[RealtimeOutput]) -> Vec<String> {
        out.iter()
            .map(|o| match o {
                RealtimeOutput::Show { seg, .. } => format!("show {:?}: {}", seg.speaker, seg.text),
                RealtimeOutput::Retract { key } => format!("retract {key}"),
                RealtimeOutput::Interim { speaker, text } => format!("interim {speaker:?}: {text}"),
            })
            .collect()
    }

    #[test]
    fn own_words_followed_by_bleed_keep_only_the_own_words() {
        let mut s = BleedSuppressor::default();
        let far = "yes we handle about forty thousand calls a day mostly english and spanish";
        s.push(timed_seg(far, 6_000.0, 13_000.0, SPEAKER_OUT_LABEL), 13_000.0);
        let mic = format!("thanks for joining today i hear you are evaluating speech to text {far}");
        // Own words 0-5.6 s, then the bleed copy 80 ms behind the far side.
        let mut seg = timed_seg("thanks for joining today i hear you are evaluating speech to text", 0.0, 5_600.0, MIC_IN_LABEL);
        let mut bleed = timed(far, 6_080.0, 13_080.0);
        let n = seg.words.len();
        bleed.iter_mut().for_each(|w| w.src += n);
        seg.words.extend(bleed);
        seg.text = mic;
        seg.end_ms = 13_080.0;
        let out = s.push(seg, 13_500.0);
        assert_eq!(
            texts(&out),
            vec!["show Some(\"Mic-In\"): thanks for joining today i hear you are evaluating speech to text"]
        );
    }

    #[test]
    fn a_shown_line_whose_end_turns_out_to_be_bleed_is_replaced_by_its_own_words() {
        let mut s = BleedSuppressor::default();
        let mut seg = timed_seg("got it are those calls recorded", 0.0, 2_000.0, MIC_IN_LABEL);
        let mut bleed = timed("both ideally real time for agent assist", 2_100.0, 5_100.0);
        let n = seg.words.len();
        bleed.iter_mut().for_each(|w| w.src += n);
        seg.words.extend(bleed);
        seg.text = "got it are those calls recorded both ideally real time for agent assist".into();
        seg.end_ms = 5_100.0;
        let shown = s.push(seg, 5_200.0);
        assert_eq!(shown.len(), 1, "no far-side final yet: shown whole");
        let RealtimeOutput::Show { key, .. } = shown[0] else { panic!() };
        let out = s.push(timed_seg("both ideally real time for agent assist", 2_020.0, 5_020.0, SPEAKER_OUT_LABEL), 5_600.0);
        assert_eq!(
            texts(&out),
            vec![
                "show Some(\"Speaker-Out\"): both ideally real time for agent assist".to_string(),
                format!("retract {key}"),
                "show Some(\"Mic-In\"): got it are those calls recorded".to_string(),
            ]
        );
    }

    #[test]
    fn bleed_at_the_start_is_trimmed_too() {
        let mut s = BleedSuppressor::default();
        s.push(timed_seg("and who would be our point of contact", 0.0, 3_000.0, SPEAKER_OUT_LABEL), 3_000.0);
        let mut seg = timed_seg("and who would be our point of contact", 80.0, 3_080.0, MIC_IN_LABEL);
        let mut own = timed("that would be me for the pilot", 3_500.0, 6_000.0);
        let n = seg.words.len();
        own.iter_mut().for_each(|w| w.src += n);
        seg.words.extend(own);
        seg.text = "and who would be our point of contact that would be me for the pilot".into();
        seg.end_ms = 6_000.0;
        let out = s.push(seg, 6_200.0);
        assert_eq!(texts(&out), vec!["show Some(\"Mic-In\"): that would be me for the pilot"]);
    }

    /// Review round 1: two own words then eight echoed ones pass the whole-line
    /// 80% rule; the reply must survive, not vanish with the bleed.
    #[test]
    fn a_short_reply_followed_by_a_long_echo_keeps_the_reply() {
        let mut s = BleedSuppressor::default();
        let far = "both ideally real time for agent assist and batch";
        s.push(timed_seg(far, 1_000.0, 5_000.0, SPEAKER_OUT_LABEL), 5_000.0);
        let mut seg = timed_seg("got it", 0.0, 900.0, MIC_IN_LABEL);
        let mut bleed = timed(far, 1_080.0, 5_080.0);
        bleed.iter_mut().for_each(|w| w.src += 2);
        seg.words.extend(bleed);
        seg.text = format!("got it {far}");
        assert!(is_bleed_of(&seg, &[&timed_seg(far, 1_000.0, 5_000.0, SPEAKER_OUT_LABEL)]), "the case: whole-line bleed");
        assert_eq!(texts(&s.push(seg, 5_500.0)), vec!["show Some(\"Mic-In\"): got it"]);
    }

    /// Review round 1: an echo whose timings sit ~1 s off the far side's still
    /// matches as a run, so its cut must use the same tolerance.
    #[test]
    fn an_echo_a_second_off_is_still_trimmed() {
        let mut s = BleedSuppressor::default();
        let far = "can you send me the soc two report";
        s.push(timed_seg(far, 4_000.0, 7_000.0, SPEAKER_OUT_LABEL), 7_000.0);
        let mut seg = timed_seg("let me check with the team first", 0.0, 3_500.0, MIC_IN_LABEL);
        let mut bleed = timed(far, 5_000.0, 8_000.0);
        let n = seg.words.len();
        bleed.iter_mut().for_each(|w| w.src += n);
        seg.words.extend(bleed);
        seg.text = format!("let me check with the team first {far}");
        assert_eq!(texts(&s.push(seg, 8_500.0)), vec!["show Some(\"Mic-In\"): let me check with the team first"]);
    }

    /// Review round 2: a far-side "yes" near the user's "yes absolutely" must
    /// not take the reply down with the echo that follows it.
    #[test]
    fn a_reply_sharing_one_word_with_the_far_side_survives() {
        let mut s = BleedSuppressor::default();
        let far = "yes and we also need batch for compliance review";
        s.push(timed_seg(far, 1_000.0, 5_000.0, SPEAKER_OUT_LABEL), 5_000.0);
        let mut seg = timed_seg("yes absolutely", 0.0, 900.0, MIC_IN_LABEL);
        let mut bleed = timed("we also need batch for compliance review", 1_480.0, 5_080.0);
        bleed.iter_mut().for_each(|w| w.src += 2);
        seg.words.extend(bleed);
        seg.text = "yes absolutely we also need batch for compliance review".into();
        assert_eq!(texts(&s.push(seg, 5_500.0)), vec!["show Some(\"Mic-In\"): yes absolutely"]);
    }

    /// Cycle 2 finding 3: "SOC-2 absolutely" has one echoed word in two, even
    /// though "SOC-2" is two tokens.
    #[test]
    fn a_multi_token_word_counts_once_in_the_half_echoed_check() {
        let far = timed_seg("soc 2 we also need batch for compliance review today", 1_000.0, 6_000.0, SPEAKER_OUT_LABEL);
        // The reply: "SOC-2" (source word 0, tokens soc + 2) then "absolutely".
        let mk = |token: &str, src: usize, start_ms: f64, word: &str| TimedWord {
            token: token.into(),
            start_ms,
            end_ms: start_ms + 200.0,
            src,
            word: word.into(),
            confidence: None,
        };
        let keep = LabeledSegment {
            text: "SOC-2 absolutely".into(),
            start_ms: 1_000.0,
            end_ms: 1_800.0,
            confidence: 0.9,
            speaker: Some(MIC_IN_LABEL),
            turns: Vec::new(),
            words: vec![mk("soc", 0, 1_000.0, "SOC-2"), mk("2", 0, 1_000.0, "SOC-2"), mk("absolutely", 1, 1_600.0, "absolutely")],
        };
        let far_words: Vec<&TimedWord> = far.words.iter().collect();
        assert!(half_echoed_at_most(&keep, &far_words), "one of two words echoed");
    }

    #[test]
    fn a_misheard_echo_is_dropped_whole_not_left_as_a_fragment() {
        let mut s = BleedSuppressor::default();
        let far = "we handle about forty thousand calls a day mostly english";
        s.push(timed_seg(far, 0.0, 4_000.0, SPEAKER_OUT_LABEL), 4_000.0);
        let echo = "we handle about 40 000 calls a day mostly english";
        assert!(s.push(timed_seg(echo, 80.0, 4_080.0, MIC_IN_LABEL), 4_500.0).is_empty());
    }

    #[test]
    fn a_short_or_untimed_overlap_is_not_trimmed() {
        let mut s = BleedSuppressor::default();
        s.push(timed_seg("sounds good to me", 0.0, 1_500.0, SPEAKER_OUT_LABEL), 1_500.0);
        // Shares three tokens with the far side at the same time: too few to cut.
        let mut seg = timed_seg("okay great", 0.0, 800.0, MIC_IN_LABEL);
        let mut tail = timed("good to me", 900.0, 1_500.0);
        tail.iter_mut().for_each(|w| w.src += 2);
        seg.words.extend(tail);
        seg.text = "okay great good to me".into();
        let out = s.push(seg, 2_000.0);
        assert_eq!(texts(&out), vec!["show Some(\"Mic-In\"): okay great good to me"]);
        // The same words said much later (a genuine repeat) stay whole.
        let late = timed_seg("so forty thousand calls a day you said", 60_000.0, 63_000.0, MIC_IN_LABEL);
        s.push(timed_seg("we run forty thousand calls a day", 0.0, 3_000.0, SPEAKER_OUT_LABEL), 3_000.0);
        assert_eq!(texts(&s.push(late, 63_500.0)).len(), 1);
    }

    /// A mic segment with Deepgram-style word timings over its own text.
    fn timed_seg(text: &str, start_ms: f64, end_ms: f64, speaker: &'static str) -> LabeledSegment {
        let mut seg = seg(text, start_ms, end_ms, Some(speaker));
        seg.words = timed(text, start_ms, end_ms);
        seg
    }

    /// Measured live (2026-10-15): the mic's echo of "Sounds great." stayed
    /// on screen because the far side's segment ran on for another sentence.
    #[test]
    fn short_echo_at_the_start_of_a_long_far_side_segment_is_bleed() {
        let spk = timed_seg(
            "Sounds great. Before we start, I have a question about the pricing changes",
            6_600.0,
            11_000.0,
            SPEAKER_OUT_LABEL,
        );
        let mic = timed_seg("Sounds great.", 6_650.0, 7_250.0, MIC_IN_LABEL);
        assert!(is_bleed_of(&mic, &[&spk]));
    }

    /// Review finding: a nova final's window includes silence, so a short
    /// reply must be judged by its words' times, not the window. A genuine
    /// "Yeah." in a 4s final, during a far-side monologue that says "yeah"
    /// 2s earlier, is kept.
    #[test]
    fn short_reply_is_judged_by_word_time_not_the_final_window() {
        let far_text = "Yeah so the plan for the demo is next week and then we ship it";
        let spk = timed_seg(far_text, 1_000.0, 7_000.0, SPEAKER_OUT_LABEL);
        let mut mic = seg("Yeah.", 1_000.0, 5_000.0, Some(MIC_IN_LABEL));
        mic.words = timed("Yeah.", 3_000.0, 3_300.0);
        assert!(!is_bleed_of(&mic, &[&spk]));
    }

    /// Measured in the app (2026-10-15): one mic line spanned two Speaker-Out
    /// finals, so no single far-side segment contained enough of it.
    #[test]
    fn echo_split_across_far_side_finals_is_bleed() {
        let a = timed_seg("For joining. Let's walk through the quarterly road map and the", 7_900.0, 11_500.0, SPEAKER_OUT_LABEL);
        let b = timed_seg("launch timeline today.", 11_600.0, 13_000.0, SPEAKER_OUT_LABEL);
        let mic = timed_seg(
            "Let's walk through the quarterly road map and the launch timeline today.",
            9_300.0,
            13_000.0,
            MIC_IN_LABEL,
        );
        assert!(!is_bleed(&mic, &a) && !is_bleed(&mic, &b), "segment-level check alone misses it");
        assert!(is_bleed_of(&mic, &[&a, &b]));
    }

    /// Genuine speech sharing common words with the far side is kept.
    #[test]
    fn genuine_line_sharing_common_words_is_not_bleed() {
        let spk = timed_seg("I think we should move the launch to next quarter", 1_000.0, 4_000.0, SPEAKER_OUT_LABEL);
        let mic = timed_seg("I think the demo went really well today", 2_000.0, 5_000.0, MIC_IN_LABEL);
        assert!(!is_bleed_of(&mic, &[&spk]));
    }

    #[test]
    fn suppressor_retracts_mic_echo_split_across_far_side_finals() {
        let mut s = BleedSuppressor::default();
        let mic = timed_seg(
            "Let's walk through the quarterly road map and the launch timeline today.",
            9_300.0,
            13_000.0,
            MIC_IN_LABEL,
        );
        let mut outputs = s.push(mic, 0.0);
        outputs.extend(s.push(
            timed_seg("For joining. Let's walk through the quarterly road map and the", 7_900.0, 11_500.0, SPEAKER_OUT_LABEL),
            500.0,
        ));
        outputs.extend(s.push(timed_seg("launch timeline today.", 11_600.0, 13_000.0, SPEAKER_OUT_LABEL), 900.0));
        assert!(visible(&outputs).iter().all(|l| l.speaker == Some(SPEAKER_OUT_LABEL)), "{:?}", visible(&outputs));
    }

    #[test]
    fn ledger_returns_every_row_of_a_retracted_line_once() {
        let mut ledger = RetractionLedger::default();
        ledger.record(0.0, 7, vec![10, 11, 12]);
        ledger.record(0.0, 8, vec![13]);
        assert_eq!(ledger.take(7), vec![10, 11, 12]);
        assert!(ledger.take(7).is_empty(), "a line is retracted at most once");
        assert_eq!(ledger.take(8), vec![13]);
    }

    /// Literal 30s, not the constant, so a changed memory window shows here.
    #[test]
    fn ledger_forgets_lines_past_the_retraction_window() {
        let mut ledger = RetractionLedger::default();
        ledger.record(0.0, 1, vec![10]);
        ledger.record(30_001.0, 2, vec![11]);
        assert!(ledger.take(1).is_empty());
        assert_eq!(ledger.take(2), vec![11]);
    }

    /// Finding 3 (guard): acoustic bleed of a short word is near-instant.
    #[test]
    fn short_simultaneous_copy_is_still_bleed() {
        let spk = seg("Okay.", 5_000.0, 5_400.0, Some(SPEAKER_OUT_LABEL));
        let mic = seg("Okay.", 5_050.0, 5_400.0, Some(MIC_IN_LABEL));
        assert!(is_bleed(&mic, &spk));
    }

    /// Finding 6: an unexpected `channel_index` shape must not reject the whole
    /// Results message (the transcript would be lost).
    #[test]
    fn results_with_malformed_channel_index_still_parse() {
        for bad in [r#""oops""#, "null", r#"[1.5, "x"]"#, "{}"] {
            let json = format!(
                r#"{{ "type": "Results", "channel_index": {}, "start": 0.0, "duration": 1.0,
                     "is_final": true,
                     "channel": {{ "alternatives": [ {{ "transcript": "Still here.", "words": [] }} ] }} }}"#,
                bad
            );
            let msg: RealtimeMessage = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("channel_index {} rejected the message: {}", bad, e));
            assert!(nova_result_to_final(msg).is_some());
        }
    }

    // ------------------------------------------------------------------
    // Failure paths (finding 5), against a local WebSocket server
    // ------------------------------------------------------------------

    /// Serve `script` to each accepted socket in turn: Ok(messages) sends those
    /// text frames then keeps reading; Err(()) rejects the handshake.
    async fn mock_deepgram(
        script: Vec<std::result::Result<Vec<String>, ()>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}/listen", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            for step in script {
                let Ok((stream, _)) = listener.accept().await else { return };
                match step {
                    Err(()) => {
                        // Not a WebSocket upgrade: the client's handshake fails.
                        use tokio::io::AsyncWriteExt;
                        let mut stream = stream;
                        let _ = stream
                            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                            .await;
                    }
                    Ok(messages) => {
                        tokio::spawn(async move {
                            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
                            let (mut tx, mut rx) = ws.split();
                            for m in messages {
                                // "__sleep:<ms>" pauses the script instead of sending.
                                if let Some(ms) = m.strip_prefix("__sleep:") {
                                    tokio::time::sleep(Duration::from_millis(ms.parse().unwrap())).await;
                                    continue;
                                }
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                if tx.send(Message::Text(m)).await.is_err() {
                                    return;
                                }
                            }
                            while let Some(Ok(_)) = rx.next().await {}
                        });
                    }
                }
            }
        });
        (base, handle)
    }

    fn feed(layout: AudioLayout) -> mpsc::UnboundedReceiver<AudioChunk> {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for i in 0..200u64 {
                let data = vec![0.0; 1600 * layout.channels() as usize];
                let chunk = AudioChunk {
                    data,
                    sample_rate: 16_000,
                    timestamp: i as f64 * 0.1,
                    chunk_id: i,
                    device_type: DeviceType::Microphone,
                    layout,
                };
                if tx.send(chunk).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        rx
    }

    fn results(channel: usize, start: f64, text: &str) -> String {
        serde_json::json!({
            "type": "Results", "channel_index": [channel, 2], "start": start, "duration": 1.0,
            "is_final": true, "channel": { "alternatives": [ { "transcript": text, "words": [] } ] }
        })
        .to_string()
    }

    type Emitted = Arc<std::sync::Mutex<Vec<RealtimeOutput>>>;

    async fn run_against(
        endpoints: RealtimeEndpoints,
        model: String,
        layout: AudioLayout,
        emitted: Emitted,
    ) -> (std::result::Result<(), String>, Emitted) {
        run_against_slow(endpoints, model, layout, emitted, Duration::ZERO).await
    }

    /// `emit_delay` makes every emit slow, so an emit racing the return of
    /// `run_realtime_core` lands visibly after it.
    async fn run_against_slow(
        endpoints: RealtimeEndpoints,
        model: String,
        layout: AudioLayout,
        emitted: Emitted,
        emit_delay: Duration,
    ) -> (std::result::Result<(), String>, Emitted) {
        let sink = emitted.clone();
        let options = DeepgramOptions { model, ..DeepgramOptions::default() };
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_realtime_core(endpoints, "k".to_string(), options, None, feed(layout), move |s| {
                std::thread::sleep(emit_delay);
                sink.lock().unwrap().push(s)
            }),
        )
        .await
        .expect("core must return promptly on error, not wait out the 30s reader deadline");
        (result, emitted)
    }

    /// A reader error ends the session with nothing emitted afterwards.
    // Multi-threaded: on a current-thread runtime the emitter always finishes
    // before the session can resume, which would hide a missing await.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reader_error_stops_every_task_before_returning() {
        let (base, _server) = mock_deepgram(vec![Ok(vec![
            results(1, 0.0, "Far side line here."),
            results(0, 10.0, "Mic line here."),
            serde_json::json!({ "type": "Error", "description": "boom" }).to_string(),
        ])])
        .await;
        let endpoints = RealtimeEndpoints { nova: base.clone(), flux: base, tag_lane_side: false };
        let (result, emitted) = run_against_slow(
            endpoints,
            "nova-3".into(),
            AudioLayout::MicSystemStereo,
            Emitted::default(),
            Duration::from_millis(300),
        )
        .await;
        assert!(result.unwrap_err().contains("boom"));
        let at_return = emitted.lock().unwrap().len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(emitted.lock().unwrap().len(), at_return, "emit after return");
        let texts: Vec<String> = visible(&emitted.lock().unwrap()).iter().map(|s| s.text.clone()).collect();
        assert!(texts.contains(&"Mic line here.".to_string()), "mic line emitted before return: {:?}", texts);
    }

    /// A Flux lane that fails to connect leaves no task running.
    #[tokio::test]
    async fn flux_second_lane_connect_failure_spawns_nothing() {
        let (base, _server) = mock_deepgram(vec![Ok(vec![]), Err(())]).await;
        let endpoints = RealtimeEndpoints { nova: base.clone(), flux: base, tag_lane_side: false };
        let (result, emitted) = run_against(endpoints, "flux-general-en".into(), AudioLayout::MicSystemStereo, Emitted::default()).await;
        assert!(result.unwrap_err().contains("Failed to connect"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(emitted.lock().unwrap().is_empty());
    }

    /// Finding 2: a mic-only stream uses one mono socket and emits Mic-In lines
    /// immediately.
    #[tokio::test]
    async fn mic_only_stream_is_mono_and_immediate() {
        let mono_result = serde_json::json!({
            "type": "Results", "channel_index": [0, 1], "start": 0.0, "duration": 1.0,
            "is_final": true, "channel": { "alternatives": [ { "transcript": "Just me talking.", "words": [] } ] }
        })
        .to_string();
        let (base, _server) = mock_deepgram(vec![Ok(vec![
            mono_result,
            "__sleep:2000".to_string(),
            serde_json::json!({ "type": "Error", "description": "end" }).to_string(),
        ])])
        .await;
        let endpoints = RealtimeEndpoints { nova: base.clone(), flux: base, tag_lane_side: false };
        let emitted = Emitted::default();
        let session = tokio::spawn(run_against(endpoints, "nova-3".into(), AudioLayout::MicOnly, emitted.clone()));
        // The Error that ends the session is 2s away; the line is already out.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let early = visible(&emitted.lock().unwrap());
        assert_eq!(early.len(), 1, "mic-only line not emitted promptly");
        assert_eq!(early[0].speaker, Some(MIC_IN_LABEL));
        let _ = session.await.unwrap();
    }

    /// Live wire test against api.deepgram.com through the production realtime
    /// core. Needs ~/.config/meetily-dgkey and MEETILY_STEREO_PCM pointing at
    /// 16 kHz s16le stereo (left = one talker, right = another).
    /// Run: cargo test --lib live_stereo_realtime -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_stereo_realtime_labels_each_channel() {
        let (key, samples) = live_inputs();
        for model in ["nova-3", "flux-general-en"] {
            let segs = live_run(&key, model, &samples, AudioLayout::MicSystemStereo).await;
            let mic: String = segs.iter().filter(|s| s.speaker == Some(MIC_IN_LABEL)).map(|s| s.text.to_lowercase()).collect();
            let spk: String = segs.iter().filter(|s| s.speaker == Some(SPEAKER_OUT_LABEL)).map(|s| s.text.to_lowercase()).collect();
            assert!(mic.contains("spacewalk") || mic.contains("space walk"), "{}: mic channel missing spacewalk: {}", model, mic);
            assert!(spk.contains("life moves pretty fast"), "{}: speaker channel missing Bueller: {}", model, spk);
            assert!(!mic.contains("life moves"), "{}: Bueller leaked into Mic-In", model);
            assert!(!spk.contains("spacewalk"), "{}: spacewalk leaked into Speaker-Out", model);
            assert!(segs.iter().all(|s| s.speaker.is_some()), "{}: unlabelled line on a stereo stream", model);
        }
    }

    /// Live bleed run: MEETILY_BLEED_PCM is stereo with the right channel's
    /// speech copied quietly into the left (speakers into the mic) plus the
    /// left talker's own words. The copy may flash as Mic-In but must not
    /// survive, and the real mic speech must.
    /// Run: MEETILY_BLEED_PCM=... cargo test --lib live_bleed -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_bleed_is_removed_and_real_mic_speech_kept() {
        let key = live_inputs_key();
        let samples = read_pcm_env("MEETILY_BLEED_PCM");
        let segs = live_run(&key, "nova-3", &samples, AudioLayout::MicSystemStereo).await;
        let mic: String = segs.iter().filter(|s| s.speaker == Some(MIC_IN_LABEL)).map(|s| s.text.to_lowercase()).collect();
        let spk: String = segs.iter().filter(|s| s.speaker == Some(SPEAKER_OUT_LABEL)).map(|s| s.text.to_lowercase()).collect();
        assert!(spk.contains("quarterly"), "far side missing: {}", spk);
        for echo in ["thanks for joining", "quarterly", "sounds great", "pricing"] {
            assert!(!mic.contains(echo), "bleed '{}' survived as Mic-In: {}", echo, mic);
        }
        assert!(mic.contains("taking notes"), "real mic speech lost: {}", mic);
    }

    /// Live mic-only run: the left channel alone as a mono MicOnly stream.
    #[tokio::test]
    #[ignore]
    async fn live_mic_only_realtime_is_mono_mic_in() {
        let (key, samples) = live_inputs();
        let left: Vec<f32> = samples.chunks_exact(2).map(|f| f[0]).collect();
        for model in ["nova-3", "flux-general-en"] {
            let segs = live_run(&key, model, &left, AudioLayout::MicOnly).await;
            assert!(!segs.is_empty());
            assert!(segs.iter().all(|s| s.speaker == Some(MIC_IN_LABEL)), "{}: {:?}", model, segs);
            let text: String = segs.iter().map(|s| s.text.to_lowercase()).collect();
            assert!(text.contains("spacewalk") || text.contains("space walk"), "{}: {}", model, text);
        }
    }

    fn live_inputs_key() -> String {
        std::fs::read_to_string(std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/meetily-dgkey"))
            .unwrap()
            .trim()
            .to_string()
    }

    fn live_inputs() -> (String, Vec<f32>) {
        (live_inputs_key(), read_pcm_env("MEETILY_STEREO_PCM"))
    }

    /// s16le PCM at the path in env var `var`, as f32 samples.
    fn read_pcm_env(var: &str) -> Vec<f32> {
        let pcm = std::fs::read(std::env::var(var).unwrap()).unwrap();
        pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect()
    }

    async fn live_run(key: &str, model: &str, samples: &[f32], layout: AudioLayout) -> Vec<LabeledSegment> {
        let (tx, rx) = mpsc::unbounded_channel();
        let collected: Emitted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = collected.clone();
        let started = std::time::Instant::now();
        let options = DeepgramOptions { model: model.to_string(), ..DeepgramOptions::default() };
        let session = tokio::spawn(run_realtime_core(
            RealtimeEndpoints::deepgram(),
            key.to_string(),
            options,
            None,
            rx,
            move |out| {
                // Audio is fed in real time from `started`, so lag is how long
                // after its last word a line reached the transcript.
                match &out {
                    RealtimeOutput::Show { key, seg: s } => println!(
                        "[live {:?}] #{:<3} lag {:>4.1}s  audio {:>5.1}s  {:<11} {}",
                        layout,
                        key,
                        started.elapsed().as_secs_f64() - s.end_ms / 1000.0,
                        s.start_ms / 1000.0,
                        s.speaker.unwrap_or("-"),
                        s.text
                    ),
                    RealtimeOutput::Retract { key } => {
                        println!("[live {:?}] #{:<3} RETRACTED at {:>5.1}s", layout, key, started.elapsed().as_secs_f64())
                    }
                    RealtimeOutput::Interim { speaker, text } if !text.is_empty() => println!(
                        "[live {:?}]  ~   at {:>5.1}s  {:<11} {}",
                        layout,
                        started.elapsed().as_secs_f64(),
                        speaker.unwrap_or("-"),
                        text
                    ),
                    RealtimeOutput::Interim { .. } => {}
                }
                sink.lock().unwrap().push(out)
            },
        ));
        // 100 ms frames, paced near realtime (faster overruns the socket).
        let per_chunk = 1600 * layout.channels() as usize;
        for (i, block) in samples.chunks(per_chunk).enumerate() {
            tx.send(AudioChunk {
                data: block.to_vec(),
                sample_rate: 16_000,
                timestamp: i as f64 * 0.1,
                chunk_id: i as u64,
                device_type: DeviceType::Microphone,
                layout,
            })
            .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        drop(tx);
        session.await.unwrap().unwrap();
        println!("[{}] ---", model);
        let segs = visible(&collected.lock().unwrap());
        segs
    }
}
