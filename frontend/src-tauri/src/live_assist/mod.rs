//! Live agent-assist: while a call records, short suggestion cards based on
//! what the other side (Speaker-Out) just said, for the person on Mic-In.
//!
//! Driven from Rust, not the webview: `start_session` registers listeners on
//! `transcript-update` / `transcript-retract` (the same events the transcript
//! log follows), so the trigger works while the window is hidden or another
//! page is open. Cards reach the UI as `assist-update` / `assist-withdraw`.
//!
//! The feature is off by default (`live_assist.json` store, key `enabled`;
//! switched in Settings > Summary or the tray's "Live Assist" item). While it
//! is off no LLM call is made; turning it off mid-call cancels the call in
//! flight. Calls use the configured summary provider and model.
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
/// The Pipecat sidecar's `/control` connection while the recording runs on
/// the Pipecat engine (WORK-559). Set: the sidecar's Flux turn events trigger
/// the calls and this session only applies their cards. Unset: the Rust
/// timer triggers them, as on the Direct engine.
///
/// Each attach gets a new generation, and messages from the sidecar carry the
/// generation of the connection they came on: a message still in flight from
/// an earlier sidecar (after a fallback, or from the previous recording while
/// its sidecar shuts down) is dropped instead of reaching this session.
static SIDECAR: Mutex<Option<(u64, mpsc::UnboundedSender<String>)>> = Mutex::new(None);
static SIDECAR_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

struct Session {
    tx: mpsc::UnboundedSender<Msg>,
    listeners: Vec<tauri::EventId>,
    /// Cancelled by `stop_calls` (Stop pressed): no call starts after it,
    /// the one in flight is cancelled and its result dropped, while the
    /// session keeps following retractions through the transcript drain.
    calls_off: CancellationToken,
}

enum Msg {
    Line { seq: u64, side: Side, text: String },
    Retract(u64),
    CallDone { call_id: u64, result: Result<String, String> },
    Dismiss(String),
    Pin { id: String, pinned: bool, reply: oneshot::Sender<Result<Card, String>> },
    Cards(oneshot::Sender<Vec<Card>>),
    SidecarAttached(u64),
    SidecarDetached(u64),
    /// An event and the generation of the connection it came on.
    Sidecar(u64, SidecarEvent),
    Stop,
}

/// What the sidecar reports about its calls (`meetily_sidecar/assist.py`).
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SidecarEvent {
    AssistStarted {
        call_id: u64,
        trigger: String,
    },
    AssistCancelled {
        call_id: u64,
        reason: String,
    },
    AssistCards {
        call_id: u64,
        trigger: String,
        cards: Vec<serde_json::Value>,
        #[serde(default)]
        since_end_of_turn_ms: Option<u64>,
        #[serde(default)]
        call_ms: Option<u64>,
    },
    AssistError {
        #[serde(default)]
        call_id: Option<u64>,
        error: String,
    },
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

    let calls_off = CancellationToken::new();
    if let Some((generation, _)) = SIDECAR.lock().unwrap().as_ref() {
        let _ = tx.send(Msg::SidecarAttached(*generation));
    }
    tauri::async_runtime::spawn(run(app.clone(), rx, tx.clone(), meeting_folder, calls_off.clone()));
    *SESSION.lock().unwrap() = Some(Session { tx, listeners: vec![update_id, retract_id], calls_off });
    info!("live assist: session started (enabled={})", ENABLED.load(Ordering::SeqCst));
}

/// Stop making calls (Stop was pressed). Synchronous, so no call can start
/// after it returns, even if the session task has not run yet.
pub fn stop_calls() {
    if let Some(session) = SESSION.lock().unwrap().as_ref() {
        session.calls_off.cancel();
    }
}

/// End the session: listeners removed, pins written. Called after the
/// transcript drain, so retractions emitted by the drain still apply.
pub fn stop_session<R: Runtime>(app: &AppHandle<R>) {
    let Some(session) = SESSION.lock().unwrap().take() else {
        return;
    };
    session.calls_off.cancel();
    for id in session.listeners {
        app.unlisten(id);
    }
    let _ = session.tx.send(Msg::Stop);
    info!("live assist: session stopped");
}

fn session_tx() -> Option<mpsc::UnboundedSender<Msg>> {
    SESSION.lock().unwrap().as_ref().map(|s| s.tx.clone())
}

// ============================================================================
// PIPECAT SIDECAR BRIDGE (called by audio::pipecat)
// ============================================================================

/// A generation for the next sidecar connection (see SIDECAR).
pub fn next_sidecar_generation() -> u64 {
    SIDECAR_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// The recording's transcription now runs through the Pipecat sidecar: its
/// turn events trigger assist calls from here on.
pub fn attach_sidecar(generation: u64, control: mpsc::UnboundedSender<String>) {
    *SIDECAR.lock().unwrap() = Some((generation, control));
    if let Some(tx) = session_tx() {
        let _ = tx.send(Msg::SidecarAttached(generation));
    }
}

/// That sidecar is gone (recording over, its control connection lost, or it
/// failed and the recording fell back to Direct): the Rust timer triggers
/// calls again. A stale generation detaches nothing.
pub fn detach_sidecar(generation: u64) {
    {
        let mut sidecar = SIDECAR.lock().unwrap();
        if sidecar.as_ref().map(|(g, _)| *g) != Some(generation) {
            return;
        }
        sidecar.take();
    }
    if let Some(tx) = session_tx() {
        let _ = tx.send(Msg::SidecarDetached(generation));
    }
}

/// One JSON message from the sidecar's `/control` connection `generation`.
pub fn on_sidecar_message(generation: u64, text: &str) {
    if SIDECAR.lock().unwrap().as_ref().map(|(g, _)| *g) != Some(generation) {
        info!("live assist: dropping a message from a sidecar that is no longer attached");
        return;
    }
    match serde_json::from_str::<SidecarEvent>(text) {
        Ok(event) => {
            if let Some(tx) = session_tx() {
                let _ = tx.send(Msg::Sidecar(generation, event));
            }
        }
        // DO NOT log the message: it can hold card text built from the meeting.
        Err(e) => warn!("live assist: unreadable sidecar message ({e}; {} bytes)", text.len()),
    }
}

fn to_sidecar(message: serde_json::Value) {
    if let Some((_, tx)) = SIDECAR.lock().unwrap().as_ref() {
        let _ = tx.send(message.to_string());
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Me => "me",
        Side::Them => "them",
        Side::Unknown => "unknown",
    }
}

fn send_shown(engine: &Engine) {
    to_sidecar(serde_json::json!({ "type": "shown", "texts": engine.shown_texts() }));
}

/// The assist config for the sidecar: the summary provider, model and key
/// (the same `LlmTarget` as Direct calls and summaries), the system prompt
/// (its one home is `cards::SYSTEM_PROMPT`), and whether calls may run.
async fn sidecar_config<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> serde_json::Value {
    let llm = async {
        let state = app.try_state::<AppState>().ok_or("database not ready")?;
        let pool = state.db_manager.pool();
        let settings = SettingsRepository::get_model_config(pool)
            .await
            .map_err(|e| format!("could not read the summary model setting: {e}"))?
            .ok_or("no summary model is configured (Settings > Summary)")?;
        let target = LlmTarget::resolve(pool, &settings.provider).await?;
        let endpoint = target.ollama_endpoint.or(target.custom_openai_endpoint);
        Ok::<_, String>(serde_json::json!({
            "provider": settings.provider,
            "model": settings.model,
            "api_key": target.api_key,
            "endpoint": endpoint,
        }))
    }
    .await;
    let window = Config::default();
    let mut config = serde_json::json!({
        "type": "assist_config",
        "enabled": enabled,
        "system_prompt": cards::SYSTEM_PROMPT,
        // The sidecar's limits come from here, their one home
        // (meetily_sidecar/assist.py `Limits`).
        "limits": {
            "call_timeout_secs": CALL_TIMEOUT.as_secs_f64(),
            "max_lines": window.max_lines,
            "max_age_secs": window.max_age.as_secs_f64(),
            "max_tokens": CLAUDE_MAX_TOKENS,
        },
    });
    match llm {
        Ok(llm) => config["llm"] = llm,
        Err(e) => config["llm_error"] = serde_json::Value::String(e),
    }
    config
}

async fn run<R: Runtime>(
    app: AppHandle<R>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    tx: mpsc::UnboundedSender<Msg>,
    folder: Option<PathBuf>,
    calls_off: CancellationToken,
) {
    let mut engine = Engine::new(Config::default());
    let mut cancel: Option<CancellationToken> = None;
    // (call id, arrival of the Speaker-Out final that triggered it), for the
    // latency the user sees: final -> cards.
    let mut trigger: Option<(u64, Instant)> = None;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut was_enabled = ENABLED.load(Ordering::SeqCst);
    // Pipecat engine: the generation of the attached sidecar, which triggers
    // the calls (see SIDECAR). None: the Rust timer triggers them.
    let mut attached: Option<u64> = None;
    // What the sidecar was last told about calls being allowed.
    let mut sidecar_allowed: Option<bool> = None;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    Msg::Stop => break,
                    Msg::Line { seq, side, text } => {
                        if attached.is_some() {
                            to_sidecar(serde_json::json!({ "type": "line", "seq": seq, "side": side_name(side), "text": text }));
                        }
                        engine.on_line(seq, side, &text, Instant::now());
                    }
                    Msg::Retract(seq) => {
                        if attached.is_some() {
                            to_sidecar(serde_json::json!({ "type": "retract", "seq": seq }));
                        }
                        let had_pins = !engine.pinned().is_empty();
                        let withdrawn = engine.on_retract(seq);
                        info!("live assist: transcript line {seq} retracted, withdrawing {} card(s)", withdrawn.len());
                        if !withdrawn.is_empty() {
                            emit_withdraw(&app, withdrawn, "retracted");
                            if had_pins {
                                save_pins(folder.as_deref(), &engine.pinned());
                            }
                            if attached.is_some() {
                                send_shown(&engine);
                            }
                        }
                    }
                    Msg::SidecarAttached(generation) => {
                        info!("live assist: Pipecat engine attached; Flux turn events trigger calls");
                        attached = Some(generation);
                        // A Rust-timer call in flight is dropped: the sidecar owns calls now.
                        if let Some(token) = cancel.take() {
                            token.cancel();
                        }
                        if let Some(id) = engine.in_flight() {
                            engine.on_call_failed(id);
                        }
                        let allowed = calls_allowed(&calls_off);
                        sidecar_allowed = Some(allowed);
                        let config = sidecar_config(&app, allowed).await;
                        to_sidecar(config);
                        for line in engine.lines() {
                            to_sidecar(serde_json::json!({ "type": "line", "seq": line.seq, "side": side_name(line.side), "text": line.text }));
                        }
                        send_shown(&engine);
                    }
                    Msg::SidecarDetached(generation) => {
                        if attached == Some(generation) {
                            info!("live assist: Pipecat engine detached; the Rust timer triggers calls");
                            attached = None;
                            sidecar_allowed = None;
                        }
                    }
                    Msg::Sidecar(generation, event) => {
                        // Only the attached connection's events: one queued
                        // before a detach, or from an earlier sidecar, is dropped.
                        if attached == Some(generation) {
                            apply_sidecar_event(&app, &mut engine, event, calls_allowed(&calls_off));
                        }
                    }
                    Msg::CallDone { call_id, result } => {
                        if engine.in_flight() == Some(call_id) {
                            cancel = None;
                            if !calls_allowed(&calls_off) {
                                // A result that was already queued when assist
                                // was turned off or Stop was pressed: drop it.
                                engine.on_call_failed(call_id);
                            }
                        }
                        let since_final = trigger.filter(|(id, _)| *id == call_id).map(|(_, at)| at.elapsed());
                        apply_result(&app, &mut engine, call_id, result, since_final);
                    }
                    Msg::Dismiss(id) => {
                        let was_pinned = engine.cards().iter().any(|c| c.id == id && c.pinned);
                        if engine.dismiss(&id) {
                            if was_pinned {
                                save_pins(folder.as_deref(), &engine.pinned());
                            }
                            if attached.is_some() {
                                send_shown(&engine);
                            }
                        }
                    }
                    Msg::Pin { id, pinned, reply } => {
                        let _ = reply.send(pin_and_save(&mut engine, folder.as_deref(), &id, pinned));
                    }
                    Msg::Cards(reply) => {
                        let _ = reply.send(engine.cards().to_vec());
                    }
                }
            }
            _ = tick.tick() => {}
        }

        let allowed = calls_allowed(&calls_off);
        if !allowed {
            if let Some(token) = cancel.take() {
                token.cancel();
                if let Some(id) = engine.in_flight() {
                    info!("live assist: calls stopped, cancelling call {id}");
                    engine.on_call_failed(id);
                }
            }
        }
        // The status reports the setting, not Stop: the UI mirrors it.
        let enabled = ENABLED.load(Ordering::SeqCst);
        if enabled != was_enabled {
            emit_status(&app, if enabled { "idle" } else { "off" }, None);
            was_enabled = enabled;
        }
        if attached.is_some() && sidecar_allowed != Some(allowed) {
            // Turning assist off, or Stop, cancels the sidecar's call in flight.
            to_sidecar(serde_json::json!({ "type": "assist_enabled", "enabled": allowed }));
            sidecar_allowed = Some(allowed);
        }
        if let Some(request) = engine.poll(Instant::now(), allowed && attached.is_none()) {
            let token = calls_off.child_token();
            cancel = Some(token.clone());
            let prompt = cards::user_prompt(
                request.lines.iter().map(|l| (l.side, l.text.as_str())),
                request.shown.iter().map(String::as_str),
            );
            let call_id = request.call_id;
            let last_final = request.lines.iter().rev().find(|l| l.side == Side::Them).map(|l| l.at);
            trigger = last_final.map(|at| (call_id, at));
            info!(
                "live assist: call {call_id} with {} line(s), {} ms after the Speaker-Out final",
                request.lines.len(),
                last_final.map_or(0, |at| at.elapsed().as_millis())
            );
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

/// Calls may run: assist is on and Stop has not been pressed.
fn calls_allowed(calls_off: &CancellationToken) -> bool {
    ENABLED.load(Ordering::SeqCst) && !calls_off.is_cancelled()
}

/// Apply one of the sidecar's call reports. Cards that arrive after assist
/// was turned off or Stop was pressed are dropped, as on the Direct engine.
fn apply_sidecar_event<R: Runtime>(app: &AppHandle<R>, engine: &mut Engine, event: SidecarEvent, allowed: bool) {
    match event {
        SidecarEvent::AssistStarted { call_id, trigger } => {
            info!("live assist: sidecar call {call_id} started ({trigger})");
            if allowed {
                emit_status(app, "thinking", None);
            }
        }
        SidecarEvent::AssistCancelled { call_id, reason } => {
            info!("live assist: sidecar call {call_id} cancelled ({reason})");
            if allowed {
                emit_status(app, "idle", None);
            }
        }
        SidecarEvent::AssistError { call_id, error } => {
            warn!("live assist: sidecar call {call_id:?} failed: {error}");
            if allowed {
                emit_status(app, "error", Some(error));
            }
        }
        SidecarEvent::AssistCards { call_id, trigger, cards, since_end_of_turn_ms, call_ms } => {
            if !allowed {
                info!("live assist: dropping sidecar call {call_id}'s cards (assist off or stopped)");
                return;
            }
            // Re-validate through the Rust parser (kinds, word limit, count).
            let raw = parse_cards(&serde_json::json!({ "cards": cards }).to_string()).unwrap_or_default();
            let applied = engine.on_external_cards(call_id, raw, chrono::Utc::now().timestamp_millis());
            info!(
                "live assist: sidecar call {call_id} ({trigger}) added {} card(s), {} ms after the Speaker-Out end of turn (call {} ms)",
                applied.added.len(),
                since_end_of_turn_ms.map_or("?".to_string(), |v| v.to_string()),
                call_ms.map_or("?".to_string(), |v| v.to_string()),
            );
            if !applied.added.is_empty() {
                if let Err(e) = app.emit(ASSIST_UPDATE_EVENT, serde_json::json!({ "cards": applied.added })) {
                    error!("live assist: failed to emit cards: {e}");
                }
            }
            emit_withdraw(app, applied.evicted, "evicted");
            send_shown(engine);
            emit_status(app, "idle", None);
        }
    }
}

fn apply_result<R: Runtime>(
    app: &AppHandle<R>,
    engine: &mut Engine,
    call_id: u64,
    result: Result<String, String>,
    since_final: Option<Duration>,
) {
    if engine.in_flight() != Some(call_id) {
        // Cancelled (assist turned off, or stopped) before it returned.
        return;
    }
    let output = match result {
        Ok(output) => output,
        Err(e) => {
            warn!("live assist: call {call_id} failed: {e}");
            engine.on_call_failed(call_id);
            emit_status(app, "error", Some(e));
            return;
        }
    };
    let raw = match parse_cards(&output) {
        Ok(raw) => raw,
        Err(e) => {
            // Malformed output is that turn's "no cards", not an error state.
            // DO NOT log the output itself: it can echo what was said in the
            // meeting, and this log is kept on disk.
            warn!("live assist: call {call_id}: {e} ({} chars)", output.chars().count());
            Vec::new()
        }
    };
    let applied = engine.on_call_done(call_id, raw, chrono::Utc::now().timestamp_millis());
    info!(
        "live assist: call {call_id} added {} card(s), {} ms after the Speaker-Out final",
        applied.added.len(),
        since_final.map_or(0, |d| d.as_millis())
    );
    if !applied.added.is_empty() {
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

pub fn load_enabled<R: Runtime>(app: &AppHandle<R>) -> bool {
    match app.store(SETTINGS_STORE) {
        Ok(store) => store.get(ENABLED_KEY).and_then(|v| v.as_bool()).unwrap_or(false),
        Err(e) => {
            warn!("live assist: settings store unavailable ({e}); treating as off");
            false
        }
    }
}

/// Save the setting and apply it to a running session at once (turning it
/// off cancels the call in flight). Shared by Settings and the tray item.
pub fn set_enabled<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> Result<(), String> {
    let store = app.store(SETTINGS_STORE).map_err(|e| format!("settings store unavailable: {e}"))?;
    store.set(ENABLED_KEY, serde_json::Value::Bool(enabled));
    store.save().map_err(|e| format!("could not save the setting: {e}"))?;
    ENABLED.store(enabled, Ordering::SeqCst);
    info!("live assist: {}", if enabled { "enabled" } else { "disabled" });
    // A running session reports the change itself on its next tick.
    if session_tx().is_none() {
        emit_status(app, if enabled { "idle" } else { "off" }, None);
    }
    crate::tray::update_tray_menu(app);
    Ok(())
}

// ============================================================================
// PINS
// ============================================================================

#[derive(Serialize, serde::Deserialize)]
struct PinsFile {
    version: u32,
    cards: Vec<Card>,
}

/// Pin or unpin a card and write the pins. If they cannot be written, the
/// pin is undone and the error returned, so the UI never shows a pin that
/// will not be kept with the meeting.
fn pin_and_save(engine: &mut Engine, folder: Option<&Path>, id: &str, pinned: bool) -> Result<Card, String> {
    let before = engine.cards().iter().find(|c| c.id == id).map(|c| c.pinned).ok_or("card not found")?;
    let card = engine.set_pinned(id, pinned).ok_or("card not found")?;
    let saved = match folder {
        Some(folder) => write_pins(folder, &engine.pinned())
            .map_err(|e| format!("could not save pinned cards in {}: {e}", folder.display())),
        None if pinned => Err("this recording has no meeting folder, so pins cannot be kept".to_string()),
        None => Ok(()),
    };
    if let Err(e) = saved {
        engine.set_pinned(id, before);
        error!("live assist: {e}");
        return Err(e);
    }
    Ok(card)
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
pub async fn live_assist_set_enabled<R: Runtime>(app: AppHandle<R>, enabled: bool) -> Result<(), String> {
    set_enabled(&app, enabled)
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
    rx.await.map_err(|_| "not recording")?
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

    fn engine_with_one_card() -> (Engine, String) {
        use std::time::Instant;
        let t0 = Instant::now();
        let mut engine = Engine::new(Config::default());
        engine.on_line(1, Side::Them, "can you send pricing", t0);
        let call = engine.poll(t0 + std::time::Duration::from_secs(2), true).unwrap();
        let raw = vec![super::cards::RawCard { kind: CardKind::Todo, text: "Send pricing".into(), quote: String::new() }];
        let id = engine.on_call_done(call.call_id, raw, 0).added[0].id.clone();
        (engine, id)
    }

    #[test]
    fn pinning_writes_the_meeting_folder() {
        let dir = tempfile::tempdir().unwrap();
        let (mut engine, id) = engine_with_one_card();
        assert!(pin_and_save(&mut engine, Some(dir.path()), &id, true).unwrap().pinned);
        assert_eq!(read_pins(dir.path()).unwrap().len(), 1);
        assert!(!pin_and_save(&mut engine, Some(dir.path()), &id, false).unwrap().pinned);
        assert_eq!(read_pins(dir.path()), Ok(vec![]));
    }

    #[test]
    fn a_pin_that_cannot_be_saved_is_undone_and_reported() {
        let (mut engine, id) = engine_with_one_card();
        assert!(pin_and_save(&mut engine, None, &id, true).is_err(), "no meeting folder");
        assert!(engine.pinned().is_empty());
        let missing = std::path::Path::new("/nonexistent/w524/folder");
        assert!(pin_and_save(&mut engine, Some(missing), &id, true).is_err(), "write failure");
        assert!(engine.pinned().is_empty());
        assert_eq!(pin_and_save(&mut engine, None, "nope", true), Err("card not found".to_string()));
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

    /// The sidecar's control messages (`meetily_sidecar/assist.py`); the
    /// Python tests emit and check the same fixture.
    #[test]
    fn sidecar_events_match_the_fixture_shared_with_the_sidecar() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../pipecat-sidecar/tests/fixtures/control_events.json")).unwrap();
        let events = fixture["events"].as_array().unwrap();
        assert_eq!(events.len(), 5);
        for event in events {
            let parsed: SidecarEvent = serde_json::from_value(event.clone())
                .unwrap_or_else(|e| panic!("Rust cannot read the sidecar's {}: {e}", event["type"]));
            if let SidecarEvent::AssistCards { cards, .. } = parsed {
                let raw = parse_cards(&serde_json::json!({ "cards": cards }).to_string()).unwrap();
                assert_eq!(raw.len(), 1, "the card survives Rust's re-validation");
            }
        }
    }

    #[test]
    fn a_message_from_a_detached_sidecar_is_dropped() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let generation = next_sidecar_generation();
        attach_sidecar(generation, tx);
        to_sidecar(serde_json::json!({ "type": "ping" }));
        assert!(rx.try_recv().is_ok(), "the attached sidecar gets messages");
        detach_sidecar(generation + 1000);
        assert!(SIDECAR.lock().unwrap().is_some(), "a stale generation detaches nothing");
        detach_sidecar(generation);
        assert!(SIDECAR.lock().unwrap().is_none());
        to_sidecar(serde_json::json!({ "type": "ping" }));
        assert!(rx.try_recv().is_err(), "nothing goes to a detached sidecar");
        // No session and a stale generation: dropped without a panic.
        on_sidecar_message(generation, r#"{"type":"assist_started","call_id":1,"trigger":"final"}"#);
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
