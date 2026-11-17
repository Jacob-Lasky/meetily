//! Live agent-assist: while a call records, short suggestion cards based on
//! what the other side (Speaker-Out) just said, for the person on Mic-In.
//!
//! Driven from Rust, not the webview: `start_session` registers listeners on
//! `transcript-update` / `transcript-retract` (the same events the transcript
//! log follows), so the trigger works while the window is hidden or another
//! page is open. Cards reach the UI as `assist-update` / `assist-withdraw`.
//!
//! The feature is off by default (`live_assist.json` store, key `enabled`).
//! While it is off no LLM call is made; turning it off mid-call cancels the
//! call in flight. Calls use the configured summary provider and model.
//!
//! Pinned cards are written to `live-assist.json` in the meeting folder so
//! the meeting page can show them afterwards. DO NOT move them into a new DB
//! table or column: a migration this build adds cannot be opened by the
//! previous AppImage (sqlx refuses a DB with unknown applied migrations), and
//! rolling back to that build is the documented recovery path.

pub mod cards;
pub mod engine;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Listener, Manager, Runtime};
use tauri_plugin_store::StoreExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::database::repositories::meeting::MeetingsRepository;
use crate::database::repositories::setting::SettingsRepository;
use crate::state::AppState;
use crate::summary::llm_client::{generate_completion, CompletionOptions};
use crate::summary::llm_target::LlmTarget;
use cards::{parse_cards, Side};
use engine::{Card, Config, Engine};

/// New cards, newest first: `{ "cards": Card[] }`.
pub const ASSIST_UPDATE_EVENT: &str = "assist-update";
/// Cards to remove: `{ "ids": string[], "reason": "retracted" | "evicted" }`.
pub const ASSIST_WITHDRAW_EVENT: &str = "assist-withdraw";
/// `{ "state": "idle" | "thinking" | "error" | "off" | "stopped", "message"?: string }`.
pub const ASSIST_STATUS_EVENT: &str = "assist-status";

const SETTINGS_STORE: &str = "live_assist.json";
const ENABLED_KEY: &str = "enabled";
/// Pinned cards, in the meeting folder.
pub const PINS_FILE: &str = "live-assist.json";

/// Whole-call limit. A slower answer is stale by the time it lands.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// 3 cards x 25 words of JSON is ~250 tokens; headroom for a model that
/// thinks anyway after the thinking-off retry.
const CLAUDE_MAX_TOKENS: u32 = 1024;
const TICK: Duration = Duration::from_millis(100);

static ENABLED: AtomicBool = AtomicBool::new(false);
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

struct Session {
    tx: mpsc::UnboundedSender<Msg>,
    listeners: Vec<tauri::EventId>,
}

enum Msg {
    Line { seq: u64, side: Side, text: String },
    Retract(u64),
    CallDone { call_id: u64, result: Result<String, String> },
    Dismiss(String),
    Pin { id: String, pinned: bool, reply: oneshot::Sender<Option<Card>> },
    Cards(oneshot::Sender<Vec<Card>>),
    Stop,
}

#[derive(Serialize, Clone)]
struct StatusPayload<'a> {
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn emit_status<R: Runtime>(app: &AppHandle<R>, state: &str, message: Option<String>) {
    if let Err(e) = app.emit(ASSIST_STATUS_EVENT, StatusPayload { state, message }) {
        error!("live assist: failed to emit status: {e}");
    }
}

fn emit_withdraw<R: Runtime>(app: &AppHandle<R>, ids: Vec<String>, reason: &str) {
    if ids.is_empty() {
        return;
    }
    if let Err(e) = app.emit(ASSIST_WITHDRAW_EVENT, serde_json::json!({ "ids": ids, "reason": reason })) {
        error!("live assist: failed to emit withdraw: {e}");
    }
}

// ============================================================================
// SESSION LIFECYCLE (called by recording_commands)
// ============================================================================

/// Start following the transcript for a new recording. Safe to call while a
/// session exists (the old one is stopped first).
pub fn start_session<R: Runtime>(app: &AppHandle<R>, meeting_folder: Option<PathBuf>) {
    stop_session(app);
    ENABLED.store(load_enabled(app), Ordering::SeqCst);

    let (tx, rx) = mpsc::unbounded_channel();
    let update_tx = tx.clone();
    let update_id = app.listen("transcript-update", move |event: tauri::Event| {
        let Ok(update) = serde_json::from_str::<crate::audio::TranscriptUpdate>(event.payload()) else {
            return;
        };
        if update.is_partial {
            return;
        }
        let _ = update_tx.send(Msg::Line {
            seq: update.sequence_id,
            side: Side::from_speaker(update.speaker.as_deref()),
            text: update.text,
        });
    });
    let retract_tx = tx.clone();
    let retract_id = app.listen(crate::audio::deepgram::TRANSCRIPT_RETRACT_EVENT, move |event: tauri::Event| {
        if let Ok(r) = serde_json::from_str::<crate::audio::deepgram::TranscriptRetract>(event.payload()) {
            let _ = retract_tx.send(Msg::Retract(r.sequence_id));
        }
    });

    tauri::async_runtime::spawn(run(app.clone(), rx, tx.clone(), meeting_folder));
    *SESSION.lock().unwrap() = Some(Session { tx, listeners: vec![update_id, retract_id] });
    info!("live assist: session started (enabled={})", ENABLED.load(Ordering::SeqCst));
}

/// Stop the session: no more calls, the one in flight is cancelled, pins are
/// written. Called as Stop is pressed, before the transcript drain.
pub fn stop_session<R: Runtime>(app: &AppHandle<R>) {
    let Some(session) = SESSION.lock().unwrap().take() else {
        return;
    };
    for id in session.listeners {
        app.unlisten(id);
    }
    let _ = session.tx.send(Msg::Stop);
    info!("live assist: session stopped");
}

fn session_tx() -> Option<mpsc::UnboundedSender<Msg>> {
    SESSION.lock().unwrap().as_ref().map(|s| s.tx.clone())
}

async fn run<R: Runtime>(
    app: AppHandle<R>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    tx: mpsc::UnboundedSender<Msg>,
    folder: Option<PathBuf>,
) {
    let mut engine = Engine::new(Config::default());
    let mut cancel: Option<CancellationToken> = None;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut was_enabled = ENABLED.load(Ordering::SeqCst);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    Msg::Stop => break,
                    Msg::Line { seq, side, text } => engine.on_line(seq, side, &text, Instant::now()),
                    Msg::Retract(seq) => {
                        let had_pins = !engine.pinned().is_empty();
                        let withdrawn = engine.on_retract(seq);
                        if !withdrawn.is_empty() {
                            info!("live assist: transcript line {seq} retracted, withdrawing {} card(s)", withdrawn.len());
                            emit_withdraw(&app, withdrawn, "retracted");
                            if had_pins {
                                save_pins(folder.as_deref(), &engine.pinned());
                            }
                        }
                    }
                    Msg::CallDone { call_id, result } => {
                        if engine.in_flight() == Some(call_id) {
                            cancel = None;
                        }
                        apply_result(&app, &mut engine, call_id, result);
                    }
                    Msg::Dismiss(id) => {
                        let was_pinned = engine.cards().iter().any(|c| c.id == id && c.pinned);
                        if engine.dismiss(&id) && was_pinned {
                            save_pins(folder.as_deref(), &engine.pinned());
                        }
                    }
                    Msg::Pin { id, pinned, reply } => {
                        let card = engine.set_pinned(&id, pinned);
                        if card.is_some() {
                            save_pins(folder.as_deref(), &engine.pinned());
                        }
                        let _ = reply.send(card);
                    }
                    Msg::Cards(reply) => {
                        let _ = reply.send(engine.cards().to_vec());
                    }
                }
            }
            _ = tick.tick() => {}
        }

        let enabled = ENABLED.load(Ordering::SeqCst);
        if !enabled {
            if let Some(token) = cancel.take() {
                token.cancel();
                if let Some(id) = engine.in_flight() {
                    info!("live assist: turned off, cancelling call {id}");
                    engine.on_call_failed(id);
                }
            }
        }
        if enabled != was_enabled {
            emit_status(&app, if enabled { "idle" } else { "off" }, None);
            was_enabled = enabled;
        }
        if let Some(request) = engine.poll(Instant::now(), enabled) {
            let token = CancellationToken::new();
            cancel = Some(token.clone());
            let prompt = cards::user_prompt(request.lines.iter().map(|l| (l.side, l.text.as_str())));
            let call_id = request.call_id;
            info!("live assist: call {call_id} with {} line(s)", request.lines.len());
            emit_status(&app, "thinking", None);
            let app = app.clone();
            let tx = tx.clone();
            tauri::async_runtime::spawn(async move {
                let started = Instant::now();
                let result = match tokio::time::timeout(CALL_TIMEOUT, call_llm(&app, &prompt, &token)).await {
                    Ok(result) => result,
                    Err(_) => Err(format!("timed out after {} s", CALL_TIMEOUT.as_secs())),
                };
                info!("live assist: call {call_id} finished in {} ms (ok={})", started.elapsed().as_millis(), result.is_ok());
                let _ = tx.send(Msg::CallDone { call_id, result });
            });
        }
    }

    if let Some(token) = cancel.take() {
        token.cancel();
    }
    save_pins(folder.as_deref(), &engine.pinned());
    emit_status(&app, "stopped", None);
}

fn apply_result<R: Runtime>(app: &AppHandle<R>, engine: &mut Engine, call_id: u64, result: Result<String, String>) {
    let output = match result {
        Ok(output) => output,
        Err(e) => {
            if engine.in_flight() == Some(call_id) {
                warn!("live assist: call {call_id} failed: {e}");
                engine.on_call_failed(call_id);
                emit_status(app, "error", Some(e));
            }
            return;
        }
    };
    let raw = match parse_cards(&output) {
        Ok(raw) => raw,
        Err(e) => {
            // Malformed output is that turn's "no cards", not an error state.
            warn!("live assist: call {call_id}: {e}; output starts {:?}", output.chars().take(120).collect::<String>());
            Vec::new()
        }
    };
    let applied = engine.on_call_done(call_id, raw, chrono::Utc::now().timestamp_millis());
    if !applied.added.is_empty() {
        info!("live assist: call {call_id} added {} card(s)", applied.added.len());
        if let Err(e) = app.emit(ASSIST_UPDATE_EVENT, serde_json::json!({ "cards": applied.added })) {
            error!("live assist: failed to emit cards: {e}");
        }
    }
    emit_withdraw(app, applied.evicted, "evicted");
    emit_status(app, "idle", None);
}

async fn call_llm<R: Runtime>(app: &AppHandle<R>, prompt: &str, cancel: &CancellationToken) -> Result<String, String> {
    let state = app.try_state::<AppState>().ok_or("database not ready")?;
    let pool = state.db_manager.pool();
    let settings = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| format!("could not read the summary model setting: {e}"))?
        .ok_or("no summary model is configured (Settings > Summary)")?;
    let target = LlmTarget::resolve(pool, &settings.provider).await?;
    let app_data_dir = app.path().app_data_dir().ok();
    let options = CompletionOptions {
        claude_max_tokens: CLAUDE_MAX_TOKENS,
        claude_thinking_off: true,
        timeout: CALL_TIMEOUT,
    };
    let completion = generate_completion(
        &HTTP,
        &target.provider,
        &settings.model,
        &target.api_key,
        cards::SYSTEM_PROMPT,
        prompt,
        target.ollama_endpoint.as_deref(),
        target.custom_openai_endpoint.as_deref(),
        target.custom_openai_max_tokens,
        target.custom_openai_temperature,
        target.custom_openai_top_p,
        app_data_dir.as_ref(),
        Some(cancel),
        &options,
    )
    .await?;
    Ok(completion.content)
}

// ============================================================================
// SETTING
// ============================================================================

fn load_enabled<R: Runtime>(app: &AppHandle<R>) -> bool {
    match app.store(SETTINGS_STORE) {
        Ok(store) => store.get(ENABLED_KEY).and_then(|v| v.as_bool()).unwrap_or(false),
        Err(e) => {
            warn!("live assist: settings store unavailable ({e}); treating as off");
            false
        }
    }
}

// ============================================================================
// PINS
// ============================================================================

#[derive(Serialize, serde::Deserialize)]
struct PinsFile {
    version: u32,
    cards: Vec<Card>,
}

fn save_pins(folder: Option<&Path>, pinned: &[Card]) {
    let Some(folder) = folder else {
        if !pinned.is_empty() {
            warn!("live assist: recording has no meeting folder; {} pinned card(s) will not be kept", pinned.len());
        }
        return;
    };
    if let Err(e) = write_pins(folder, pinned) {
        error!("live assist: failed to save pinned cards in {}: {e}", folder.display());
    }
}

/// Write the pins atomically (temp file + rename in the same folder). No
/// pins and no file yet: nothing is written.
fn write_pins(folder: &Path, pinned: &[Card]) -> std::io::Result<()> {
    let path = folder.join(PINS_FILE);
    if pinned.is_empty() && !path.exists() {
        return Ok(());
    }
    let body = serde_json::to_vec_pretty(&PinsFile { version: 1, cards: pinned.to_vec() })?;
    let tmp = folder.join(format!(".{PINS_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

fn read_pins(folder: &Path) -> Result<Vec<Card>, String> {
    let path = folder.join(PINS_FILE);
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    serde_json::from_slice::<PinsFile>(&body)
        .map(|f| f.cards)
        .map_err(|e| format!("could not parse {}: {e}", path.display()))
}

// ============================================================================
// COMMANDS
// ============================================================================

#[tauri::command]
pub async fn live_assist_get_enabled<R: Runtime>(app: AppHandle<R>) -> bool {
    load_enabled(&app)
}

#[tauri::command]
pub async fn live_assist_set_enabled<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| format!("settings store unavailable: {e}"))?;
    store.set(ENABLED_KEY, serde_json::Value::Bool(enabled));
    store.save().map_err(|e| format!("could not save the setting: {e}"))?;
    ENABLED.store(enabled, Ordering::SeqCst);
    info!("live assist: {}", if enabled { "enabled" } else { "disabled" });
    if session_tx().is_none() {
        emit_status(&app, if enabled { "idle" } else { "off" }, None);
    }
    Ok(())
}

/// Cards of the running session (empty when not recording), newest first.
#[tauri::command]
pub async fn live_assist_get_cards() -> Vec<Card> {
    let Some(tx) = session_tx() else { return Vec::new() };
    let (reply, rx) = oneshot::channel();
    if tx.send(Msg::Cards(reply)).is_err() {
        return Vec::new();
    }
    rx.await.unwrap_or_default()
}

#[tauri::command]
pub async fn live_assist_dismiss(id: String) {
    if let Some(tx) = session_tx() {
        let _ = tx.send(Msg::Dismiss(id));
    }
}

/// Pin or unpin a card of the running session; returns the updated card.
#[tauri::command]
pub async fn live_assist_set_pinned(id: String, pinned: bool) -> Result<Card, String> {
    let tx = session_tx().ok_or("not recording")?;
    let (reply, rx) = oneshot::channel();
    tx.send(Msg::Pin { id, pinned, reply }).map_err(|_| "not recording")?;
    rx.await.map_err(|_| "not recording")?.ok_or_else(|| "card not found".to_string())
}

/// Pinned cards saved with a meeting (empty when it has none).
#[tauri::command]
pub async fn live_assist_get_pinned(state: tauri::State<'_, AppState>, meeting_id: String) -> Result<Vec<Card>, String> {
    let meeting = MeetingsRepository::get_meeting_metadata(state.db_manager.pool(), &meeting_id)
        .await
        .map_err(|e| format!("Failed to load meeting metadata: {e}"))?
        .ok_or_else(|| format!("Meeting not found: {meeting_id}"))?;
    match meeting.folder_path.filter(|p| !p.trim().is_empty()) {
        Some(folder) => read_pins(Path::new(&folder)),
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::cards::CardKind;
    use super::*;

    fn card(id: &str) -> Card {
        Card {
            id: id.into(),
            kind: CardKind::Todo,
            text: "Send the pricing sheet".into(),
            quote: "can you send pricing".into(),
            source_seq: 7,
            pinned: true,
            created_at_ms: 1_763_400_000_000,
        }
    }

    #[test]
    fn pins_round_trip_through_the_meeting_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pins(dir.path()), Ok(vec![]), "no file: no pins");
        write_pins(dir.path(), &[card("a"), card("b")]).unwrap();
        assert_eq!(read_pins(dir.path()).unwrap(), vec![card("a"), card("b")]);
        write_pins(dir.path(), &[]).unwrap();
        assert_eq!(read_pins(dir.path()), Ok(vec![]), "unpinning the last card empties the file");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from(PINS_FILE)], "no temp file left behind");
    }

    #[test]
    fn no_pins_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        write_pins(dir.path(), &[]).unwrap();
        assert!(!dir.path().join(PINS_FILE).exists());
    }

    #[test]
    fn a_corrupt_pins_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PINS_FILE), b"{oops").unwrap();
        assert!(read_pins(dir.path()).is_err());
    }

    #[test]
    fn card_json_matches_the_frontend_contract() {
        let v = serde_json::to_value(card("a")).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "id": "a", "kind": "todo", "text": "Send the pricing sheet",
                "quote": "can you send pricing", "source_seq": 7, "pinned": true,
                "created_at_ms": 1_763_400_000_000_i64
            })
        );
        let ts = include_str!("../../../src/lib/live-assist.ts");
        for event in [ASSIST_UPDATE_EVENT, ASSIST_WITHDRAW_EVENT, ASSIST_STATUS_EVENT] {
            assert!(ts.contains(&format!("'{event}'")), "frontend does not listen for {event}");
        }
        for field in ["source_seq", "created_at_ms", "pinned", "quote"] {
            assert!(ts.contains(field), "frontend Card type lacks {field}");
        }
    }
}
