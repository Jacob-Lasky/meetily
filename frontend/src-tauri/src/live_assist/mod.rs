//! Live agent-assist: while a call records, short suggestion cards based on
//! what the other side (Speaker-Out) just said, for the person on Mic-In.
//!
//! Driven from Rust, not the webview: `start_session` registers listeners on
//! `transcript-update` / `transcript-retract` (the same events the transcript
//! log follows), so the trigger works while the window is hidden or another
//! page is open. Cards reach the UI as `assist-update` / `assist-withdraw`.
//!
//! The feature is off by default (`live_assist.json` store, key `enabled`;
//! switched in Settings › AI Assist or the tray's "Live Assist" item). While
//! it is off no LLM call is made; turning it off mid-call cancels the call in
//! flight. Calls use the AI Assist model (default: the summary model) and
//! the prompt `cards::system_prompt` builds from Settings › AI Assist and
//! the meeting brief (`settings`).
//!
//! Pinned cards are written to `live-assist.json` in the meeting folder so
//! the meeting page can show them afterwards. DO NOT move them into a new DB
//! table or column: a migration this build adds cannot be opened by the
//! previous AppImage (sqlx refuses a DB with unknown applied migrations), and
//! rolling back to that build is the documented recovery path.

pub mod cards;
pub mod engine;
pub mod mcp_probe;
pub mod settings;

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
use cards::{keep_kinds, parse_cards, PromptParts, Side};
use engine::{Card, Config, Engine};
use settings::{AssistSettings, McpTransport, BRIEF_KEY, SETTINGS_KEY};

/// New cards, newest first: `{ "cards": Card[] }`.
pub const ASSIST_UPDATE_EVENT: &str = "assist-update";
/// Cards to remove: `{ "ids": string[], "reason": "retracted" | "evicted" }`.
pub const ASSIST_WITHDRAW_EVENT: &str = "assist-withdraw";
/// `{ "state": "idle" | "thinking" | "error" | "off" | "stopped", "message"?: string }`.
pub const ASSIST_STATUS_EVENT: &str = "assist-status";
/// The research lane (Pipecat engine): `{ "state": "started" | "done" | "error",
/// "question": string, "message"?: string, "cards"?: number }`.
pub const ASSIST_RESEARCH_EVENT: &str = "assist-research";

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
    /// The brief as it was when Stop was pressed: the one this call ran
    /// with. Teardown keeps that copy with the meeting, not whatever the box
    /// holds by then (a brief typed for the next call during the drain).
    brief_at_stop: std::sync::Arc<Mutex<Option<String>>>,
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
    /// Settings › AI Assist or the meeting brief changed: the attached
    /// sidecar gets the new config (the Direct engine reads them per call).
    Reconfigure,
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
    ResearchStarted {
        research_id: u64,
        question: String,
    },
    ResearchCards {
        research_id: u64,
        question: String,
        cards: Vec<serde_json::Value>,
        /// Names of the MCP servers whose tools the lookup called.
        #[serde(default)]
        sources: Vec<String>,
        #[serde(default)]
        call_ms: Option<u64>,
    },
    ResearchError {
        #[serde(default)]
        research_id: Option<u64>,
        #[serde(default)]
        question: Option<String>,
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
    let brief_at_stop = std::sync::Arc::new(Mutex::new(None));
    tauri::async_runtime::spawn(run(
        app.clone(),
        rx,
        tx.clone(),
        meeting_folder,
        calls_off.clone(),
        brief_at_stop.clone(),
    ));
    *SESSION.lock().unwrap() =
        Some(Session { tx, listeners: vec![update_id, retract_id], calls_off, brief_at_stop });
    info!("live assist: session started (enabled={})", ENABLED.load(Ordering::SeqCst));
}

/// Stop making calls (Stop was pressed). Synchronous, so no call can start
/// after it returns, even if the session task has not run yet. Also notes
/// the brief this call ran with (see `Session::brief_at_stop`).
pub fn stop_calls<R: Runtime>(app: &AppHandle<R>) {
    let stopped = stop_then_read(
        || {
            let session = SESSION.lock().unwrap();
            let session = session.as_ref()?;
            session.calls_off.cancel();
            Some(session.brief_at_stop.clone())
        },
        || load_brief(app),
    );
    if let Some((brief_at_stop, brief)) = stopped {
        brief_at_stop.lock().unwrap().get_or_insert(brief);
    }
}

/// Under `CONTEXT_SAVE`, from before `stop` (which cancels the calls)
/// through `read` (the brief): a brief saved before Stop was already sent to
/// the running call, so it is that call's brief; one saved after waits until
/// the read is done and belongs to the next call. Lock order is the one
/// `set_brief` and `save_settings` use: `CONTEXT_SAVE`, then `SESSION`.
/// `stop` returning None (no session) skips the read.
fn stop_then_read<T>(stop: impl FnOnce() -> Option<T>, read: impl FnOnce() -> String) -> Option<(T, String)> {
    let _guard = CONTEXT_SAVE.lock().unwrap();
    let stopped = stop()?;
    Some((stopped, read()))
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

/// Research lane limits, sent to the sidecar (`meetily_sidecar/research.py`).
/// A lookup is slower than a card by design (tools, a second model call); one
/// still useful has to land while the topic is live.
const RESEARCH_TIMEOUT: Duration = Duration::from_secs(45);
/// Each tool round is a model call (~1.5 s); measured 2026-11-18, DeepHive
/// lookups that searched 5 times took 10 s.
const RESEARCH_MAX_TOOL_CALLS: u32 = 3;

/// The provider, model and target for assist calls: the AI Assist model when
/// one is set, else the summary model (Settings › Summary).
async fn assist_target<R: Runtime>(
    app: &AppHandle<R>,
    settings: &AssistSettings,
) -> Result<(String, String, LlmTarget), String> {
    let state = app.try_state::<AppState>().ok_or("database not ready")?;
    let pool = state.db_manager.pool();
    let (provider, model) = match &settings.model {
        Some(m) => (m.provider.clone(), m.model.clone()),
        None => {
            let summary = SettingsRepository::get_model_config(pool)
                .await
                .map_err(|e| format!("could not read the summary model setting: {e}"))?
                .ok_or("no summary model is configured (Settings › Summary), and AI Assist has no model of its own")?;
            (summary.provider, summary.model)
        }
    };
    let target = LlmTarget::resolve(pool, &provider).await?;
    Ok((provider, model, target))
}

use settings::DEEPGRAM_PUBLIC;
const DEEPGRAM_PUBLIC_NAME: &str = "Deepgram, Pipecat and Daily public overview (built in)";
/// Pipecat's and Daily's public docs MCP servers (Mintlify; no sign-in).
/// Only their search tools: each also lists `submit_feedback`, which writes.
const PIPECAT_DOCS_MCP_URL: &str = "https://docs.pipecat.ai/mcp";
const PIPECAT_DOCS_TOOL: &str = "search_pipecat";
const DAILY_DOCS_MCP_URL: &str = "https://docs.daily.co/mcp";
const DAILY_DOCS_TOOL: &str = "search_daily";
/// Deepgram's hosted docs MCP server (developers.deepgram.com, "Agentic
/// Developer Tools"); it takes `Authorization: Token <Deepgram API key>`.
const DEEPGRAM_DOCS_MCP_URL: &str = "https://api.dx.deepgram.com/kapa/mcp";
const DEEPGRAM_DOCS_TOOL: &str = "search_deepgram_knowledge_sources";

/// Research would run: it is on, some server is on (a user server or the
/// built-in docs), and a kind its cards can use is on. Whether the built-in
/// docs server can connect (a Deepgram key) is settled in `research_servers`.
fn research_wanted(settings: &AssistSettings) -> bool {
    settings.research.enabled
        && (!settings.research.active_servers().is_empty()
            || settings.builtin.deepgram_docs
            || settings.builtin.pipecat_daily_docs)
        && (settings.card_kinds.answer || settings.card_kinds.fact)
}

fn prompt_parts<'a>(settings: &'a AssistSettings, brief: &'a str, research: bool) -> PromptParts<'a> {
    let builtin = settings.overview_included(brief.chars().count()).then_some((DEEPGRAM_PUBLIC_NAME, DEEPGRAM_PUBLIC));
    PromptParts {
        kinds: settings.card_kinds.enabled(),
        instructions: &settings.instructions,
        about_me: &settings.about_me,
        knowledge: builtin
            .into_iter()
            .chain(settings.knowledge.iter().filter(|k| k.enabled).map(|k| (k.name.as_str(), k.text.as_str())))
            .collect(),
        brief,
        research,
    }
}

/// The Deepgram API key saved for transcription, if any.
async fn deepgram_key<R: Runtime>(app: &AppHandle<R>) -> Option<String> {
    let state = app.try_state::<AppState>()?;
    SettingsRepository::get_transcript_api_key(state.db_manager.pool(), "deepgram")
        .await
        .ok()
        .flatten()
        .filter(|k| !k.trim().is_empty())
}

/// The built-in Deepgram docs server with the saved key, or None without one.
async fn builtin_docs_server<R: Runtime>(app: &AppHandle<R>) -> Option<settings::McpServer> {
    Some(builtin_docs_server_with(&deepgram_key(app).await?))
}

fn builtin_docs_server_with(key: &str) -> settings::McpServer {
    settings::McpServer {
        id: "builtin-deepgram-docs".into(),
        name: settings::BUILTIN_DOCS_NAME.into(),
        enabled: true,
        transport: McpTransport::Http,
        command: String::new(),
        args: Vec::new(),
        env: Default::default(),
        url: DEEPGRAM_DOCS_MCP_URL.into(),
        headers: [("Authorization".to_string(), format!("Token {}", key.trim()))].into_iter().collect(),
        allowed_tools: vec![DEEPGRAM_DOCS_TOOL.into()],
    }
}

/// The card system prompt for these settings and brief.
pub fn compose_prompt(settings: &AssistSettings, brief: &str, research: bool) -> String {
    cards::system_prompt(&prompt_parts(settings, brief, research))
}

/// The research lane's MCP servers, as the sidecar's `research.py` reads
/// them: the user's servers that are on, then the built-in docs server when
/// it is on and a Deepgram key is saved. Empty when research is not wanted.
async fn research_servers<R: Runtime>(app: &AppHandle<R>, settings: &AssistSettings) -> Vec<serde_json::Value> {
    if !research_wanted(settings) {
        return Vec::new();
    }
    let mut servers: Vec<settings::McpServer> = settings.research.active_servers().into_iter().cloned().collect();
    if settings.builtin.deepgram_docs {
        match builtin_docs_server(app).await {
            Some(docs) => servers.push(docs),
            None => info!("live assist: no Deepgram key saved; the built-in Deepgram docs server is skipped"),
        }
    }
    if settings.builtin.pipecat_daily_docs {
        servers.extend(pipecat_daily_docs_servers());
    }
    servers
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name.trim(),
                "transport": match s.transport {
                    McpTransport::Stdio => "stdio",
                    McpTransport::Http => "http",
                    McpTransport::Sse => "sse",
                },
                "command": s.command.trim(),
                "args": s.args,
                "env": s.env,
                "url": s.url.trim(),
                "headers": s.headers,
                "allowed_tools": s.allowed_tools,
            })
        })
        .collect()
}

/// The assist config for the sidecar: the AI Assist provider, model and key
/// (the same `LlmTarget` rules as Direct calls and summaries), the system
/// prompts (their one home is `cards`), the research lane's MCP servers, and
/// whether calls may run.
async fn sidecar_config<R: Runtime>(app: &AppHandle<R>, enabled: bool) -> serde_json::Value {
    let settings = load_settings(app);
    let brief = load_brief(app);
    let servers = research_servers(app, &settings).await;
    let research = !servers.is_empty();
    let llm = assist_target(app, &settings).await.map(|(provider, model, target)| {
        let endpoint = target.ollama_endpoint.or(target.custom_openai_endpoint);
        serde_json::json!({
            "provider": provider,
            "model": model,
            "api_key": target.api_key,
            "endpoint": endpoint,
        })
    });
    let window = Config::default();
    let mut config = serde_json::json!({
        "type": "assist_config",
        "enabled": enabled,
        "system_prompt": compose_prompt(&settings, &brief, research),
        // The sidecar's limits come from here, their one home
        // (meetily_sidecar/assist.py `Limits`).
        "limits": {
            "call_timeout_secs": CALL_TIMEOUT.as_secs_f64(),
            "max_lines": window.max_lines,
            "max_age_secs": window.max_age.as_secs_f64(),
            "max_tokens": CLAUDE_MAX_TOKENS,
        },
        "research": {
            "enabled": research,
            "servers": servers,
            "system_prompt": cards::research_prompt(&prompt_parts(&settings, &brief, true)),
            "timeout_secs": RESEARCH_TIMEOUT.as_secs_f64(),
            "max_tool_calls": RESEARCH_MAX_TOOL_CALLS,
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
    brief_at_stop: std::sync::Arc<Mutex<Option<String>>>,
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
                    Msg::Reconfigure => {
                        if attached.is_some() {
                            info!("live assist: AI Assist settings changed; sending the sidecar the new config");
                            let allowed = calls_allowed(&calls_off);
                            sidecar_allowed = Some(allowed);
                            let config = sidecar_config(&app, allowed).await;
                            to_sidecar(config);
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
                        let result = pin_and_save(&mut engine, folder.as_deref(), &id, pinned);
                        let unpinned = result.as_ref().is_ok_and(|c| !c.pinned);
                        let _ = reply.send(result);
                        if unpinned {
                            // An unpinned card counts toward the limit again.
                            emit_withdraw(&app, engine.evict_overflow(), "evicted");
                        }
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
    // The brief was for this call: kept with the meeting, then cleared so it
    // does not leak into the next one. Compare-and-clear: a brief typed for
    // the next call while this session wound down is left alone. It is
    // cleared only once it is safely with the meeting: with no folder, or a
    // failed write, it stays in the box rather than being lost.
    let brief = brief_at_stop.lock().unwrap().take().unwrap_or_else(|| load_brief(&app));
    let has_brief = !brief.trim().is_empty();
    let kept = match folder.as_deref() {
        Some(folder) => match write_pins(folder, &engine.pinned(), has_brief.then_some(brief.as_str())) {
            Ok(()) => true,
            Err(e) => {
                error!("live assist: failed to save pinned cards and the brief in {}: {e}", folder.display());
                false
            }
        },
        None => {
            save_pins(None, &engine.pinned());
            false
        }
    };
    if has_brief && kept {
        clear_brief_if(&app, &brief);
    }
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
            let raw = keep_kinds(raw, &load_settings(app).card_kinds.enabled());
            let applied = engine.on_external_cards(call_id, raw, None, chrono::Utc::now().timestamp_millis());
            info!(
                "live assist: sidecar call {call_id} ({trigger}) added {} card(s), {} ms after the Speaker-Out end of turn (call {} ms)",
                applied.added.len(),
                since_end_of_turn_ms.map_or("?".to_string(), |v| v.to_string()),
                call_ms.map_or("?".to_string(), |v| v.to_string()),
            );
            log_cards_for_test(&applied.added);
            if !applied.added.is_empty() {
                if let Err(e) = app.emit(ASSIST_UPDATE_EVENT, serde_json::json!({ "cards": applied.added })) {
                    error!("live assist: failed to emit cards: {e}");
                }
            }
            emit_withdraw(app, applied.evicted, "evicted");
            send_shown(engine);
            emit_status(app, "idle", None);
        }
        SidecarEvent::ResearchStarted { research_id, question } => {
            // DO NOT log the question: it is built from what was said.
            info!("live assist: research {research_id} started");
            if allowed {
                emit_research(app, "started", &question, None, None);
            }
        }
        SidecarEvent::ResearchError { research_id, question, error } => {
            warn!("live assist: research {research_id:?} failed: {error}");
            if allowed {
                emit_research(app, "error", question.as_deref().unwrap_or(""), Some(error), None);
            }
        }
        SidecarEvent::ResearchCards { research_id, question, cards, sources, call_ms } => {
            if !allowed {
                info!("live assist: dropping research {research_id}'s cards (assist off or stopped)");
                return;
            }
            let raw = parse_cards(&serde_json::json!({ "cards": cards }).to_string()).unwrap_or_default();
            let raw = keep_kinds(raw, &load_settings(app).card_kinds.enabled());
            let origin = if sources.is_empty() { "research".to_string() } else { sources.join(", ") };
            let applied = engine.on_external_cards(
                research_id,
                raw,
                Some(origin),
                chrono::Utc::now().timestamp_millis(),
            );
            info!(
                "live assist: research {research_id} added {} card(s) (call {} ms)",
                applied.added.len(),
                call_ms.map_or("?".to_string(), |v| v.to_string()),
            );
            log_cards_for_test(&applied.added);
            if !applied.added.is_empty() {
                if let Err(e) = app.emit(ASSIST_UPDATE_EVENT, serde_json::json!({ "cards": applied.added })) {
                    error!("live assist: failed to emit cards: {e}");
                }
            }
            emit_withdraw(app, applied.evicted, "evicted");
            send_shown(engine);
            emit_research(app, "done", &question, None, Some(applied.added.len()));
        }
    }
}

/// Test runs only: with `MEETILY_ASSIST_LOG_CARDS=1` in the environment,
/// each added card's kind, text and origin is logged, so a scripted live run
/// can check what the cards said. Off by default: DO NOT log card text
/// otherwise, it is built from the meeting and this log is kept on disk.
fn log_cards_for_test(cards: &[Card]) {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var("MEETILY_ASSIST_LOG_CARDS").as_deref() == Ok("1"));
    if !*ON {
        return;
    }
    for c in cards {
        info!("live assist test card: [{:?}] {} (origin: {})", c.kind, c.text, c.origin.as_deref().unwrap_or("-"));
    }
}

fn emit_research<R: Runtime>(app: &AppHandle<R>, state: &str, question: &str, message: Option<String>, cards: Option<usize>) {
    let mut payload = serde_json::json!({ "state": state, "question": question });
    if let Some(m) = message {
        payload["message"] = serde_json::Value::String(m);
    }
    if let Some(n) = cards {
        payload["cards"] = serde_json::json!(n);
    }
    if let Err(e) = app.emit(ASSIST_RESEARCH_EVENT, payload) {
        error!("live assist: failed to emit research status: {e}");
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
    let raw = keep_kinds(raw, &load_settings(app).card_kinds.enabled());
    let applied = engine.on_call_done(call_id, raw, chrono::Utc::now().timestamp_millis());
    info!(
        "live assist: call {call_id} added {} card(s), {} ms after the Speaker-Out final",
        applied.added.len(),
        since_final.map_or(0, |d| d.as_millis())
    );
    log_cards_for_test(&applied.added);
    if !applied.added.is_empty() {
        if let Err(e) = app.emit(ASSIST_UPDATE_EVENT, serde_json::json!({ "cards": applied.added })) {
            error!("live assist: failed to emit cards: {e}");
        }
    }
    emit_withdraw(app, applied.evicted, "evicted");
    emit_status(app, "idle", None);
}

async fn call_llm<R: Runtime>(app: &AppHandle<R>, prompt: &str, cancel: &CancellationToken) -> Result<String, String> {
    let settings = load_settings(app);
    let brief = load_brief(app);
    // The Direct engine has no research lane: never offer the field.
    let system = compose_prompt(&settings, &brief, false);
    let (_, model, target) = assist_target(app, &settings).await?;
    let app_data_dir = app.path().app_data_dir().ok();
    let options = CompletionOptions {
        claude_max_tokens: CLAUDE_MAX_TOKENS,
        claude_thinking_off: true,
        claude_cache_system: true,
        timeout: CALL_TIMEOUT,
    };
    let completion = generate_completion(
        &HTTP,
        &target.provider,
        &model,
        &target.api_key,
        &system,
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

/// Settings › AI Assist. A missing or unreadable object is the defaults (the
/// summary model, no context, all card kinds, no research).
pub fn load_settings<R: Runtime>(app: &AppHandle<R>) -> AssistSettings {
    let Ok(store) = app.store(SETTINGS_STORE) else { return AssistSettings::default() };
    match store.get(SETTINGS_KEY) {
        None => AssistSettings::default(),
        Some(v) => AssistSettings::from_stored(v).unwrap_or_else(|e| {
            warn!("live assist: AI Assist settings unreadable ({e}); using the defaults");
            AssistSettings::default()
        }),
    }
}

/// Validate and save Settings › AI Assist; a running call picks them up at
/// once (the Direct engine reads them per call, the sidecar is re-sent its config).
/// What a save reports back: whether the built-in overview made it into the
/// prompt with the brief stored at that moment (it steps out when it does
/// not fit, `AssistSettings::overview_included`).
#[derive(Serialize)]
pub struct SavedSettings {
    overview_included: bool,
}

pub async fn save_settings<R: Runtime>(app: &AppHandle<R>, settings: &AssistSettings) -> Result<SavedSettings, String> {
    settings.validate(load_brief(app).chars().count())?;
    if settings.model.is_some() {
        // A model of its own must be callable: same key and endpoint rules
        // as the calls themselves, checked now instead of on every turn.
        assist_target(app, settings).await.map_err(|e| format!("the AI Assist model cannot be used: {e}"))?;
    }
    // Settings and brief share one limit: check and store under one lock,
    // so a brief saved meanwhile is counted.
    let _guard = CONTEXT_SAVE.lock().unwrap();
    let brief_chars = load_brief(app).chars().count();
    settings.validate(brief_chars)?;
    let store = app.store(SETTINGS_STORE).map_err(|e| format!("settings store unavailable: {e}"))?;
    store.set(SETTINGS_KEY, serde_json::to_value(settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("could not save the AI Assist settings: {e}"))?;
    info!("live assist: AI Assist settings saved");
    reconfigure_session();
    Ok(SavedSettings { overview_included: settings.overview_included(brief_chars) })
}

pub fn load_brief<R: Runtime>(app: &AppHandle<R>) -> String {
    app.store(SETTINGS_STORE)
        .ok()
        .and_then(|store| store.get(BRIEF_KEY))
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn set_brief<R: Runtime>(app: &AppHandle<R>, brief: &str) -> Result<(), String> {
    let _guard = CONTEXT_SAVE.lock().unwrap();
    let total = load_settings(app).context_chars(brief.chars().count());
    if total > settings::MAX_CONTEXT_CHARS {
        return Err(format!(
            "about me, the enabled knowledge and the meeting brief come to {total} characters; the limit is {}",
            settings::MAX_CONTEXT_CHARS
        ));
    }
    let store = app.store(SETTINGS_STORE).map_err(|e| format!("settings store unavailable: {e}"))?;
    store.set(BRIEF_KEY, serde_json::Value::String(brief.to_string()));
    store.save().map_err(|e| format!("could not save the meeting brief: {e}"))?;
    reconfigure_session();
    Ok(())
}

/// Held while settings or the brief are checked against the shared context
/// limit and stored.
static CONTEXT_SAVE: Mutex<()> = Mutex::new(());

/// Clear the brief if it is still `expected`.
fn clear_brief_if<R: Runtime>(app: &AppHandle<R>, expected: &str) {
    let Ok(store) = app.store(SETTINGS_STORE) else { return };
    let cleared = clear_if_still(
        expected,
        || store.get(BRIEF_KEY).and_then(|v| v.as_str().map(str::to_string)),
        || {
            store.delete(BRIEF_KEY);
            if let Err(e) = store.save() {
                warn!("live assist: could not clear the meeting brief: {e}");
            }
        },
    );
    if !cleared {
        return;
    }
    // The cleared text goes with the event: a box holding anything else (a
    // brief already typed for the next call) keeps it.
    if let Err(e) = app.emit(BRIEF_CLEARED_EVENT, serde_json::json!({ "brief": expected })) {
        error!("live assist: failed to emit brief-cleared: {e}");
    }
}

/// Under `CONTEXT_SAVE`, across the read, the delete and the save: a
/// `set_brief` (which holds the same lock) cannot land between them and be
/// deleted. The only caller, the end of the session task, holds no lock.
fn clear_if_still(expected: &str, read: impl FnOnce() -> Option<String>, delete: impl FnOnce()) -> bool {
    let _guard = CONTEXT_SAVE.lock().unwrap();
    if read().as_deref() != Some(expected) {
        return false;
    }
    delete();
    true
}

/// The brief was kept with the meeting that just stopped and cleared:
/// `{ "brief": string }`, the text that was cleared.
pub const BRIEF_CLEARED_EVENT: &str = "assist-brief-cleared";

fn reconfigure_session() {
    if let Some(tx) = session_tx() {
        let _ = tx.send(Msg::Reconfigure);
    }
}

// ============================================================================
// PINS
// ============================================================================

#[derive(Serialize, serde::Deserialize)]
struct PinsFile {
    version: u32,
    cards: Vec<Card>,
    /// The meeting brief the call ran with (WORK-581), written when the
    /// recording stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    brief: Option<String>,
}

/// Pin or unpin a card and write the pins. If they cannot be written, the
/// pin is undone and the error returned, so the UI never shows a pin that
/// will not be kept with the meeting.
fn pin_and_save(engine: &mut Engine, folder: Option<&Path>, id: &str, pinned: bool) -> Result<Card, String> {
    // The whole list, not just the flag: pinning also moves the card.
    let before = engine.cards().to_vec();
    let card = engine.set_pinned(id, pinned).ok_or("card not found")?;
    let saved = match folder {
        Some(folder) => write_pins(folder, &engine.pinned(), None)
            .map_err(|e| format!("could not save pinned cards in {}: {e}", folder.display())),
        None if pinned => Err("this recording has no meeting folder, so pins cannot be kept".to_string()),
        None => Ok(()),
    };
    if let Err(e) = saved {
        engine.restore_cards(before);
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
    if let Err(e) = write_pins(folder, pinned, None) {
        error!("live assist: failed to save pinned cards in {}: {e}", folder.display());
    }
}

/// Write the pins (and, when the recording stops, the brief) atomically:
/// temp file + rename in the same folder. Nothing to keep and no file yet:
/// nothing is written. A write without a brief keeps the one on disk.
fn write_pins(folder: &Path, pinned: &[Card], brief: Option<&str>) -> std::io::Result<()> {
    let path = folder.join(PINS_FILE);
    if pinned.is_empty() && brief.is_none() && !path.exists() {
        return Ok(());
    }
    let brief = match brief {
        Some(b) => Some(b.to_string()),
        None => std::fs::read(&path)
            .ok()
            .and_then(|body| serde_json::from_slice::<PinsFile>(&body).ok())
            .and_then(|f| f.brief),
    };
    let body = serde_json::to_vec_pretty(&PinsFile { version: 1, cards: pinned.to_vec(), brief })?;
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

/// Settings › AI Assist.
#[tauri::command]
pub async fn ai_assist_get_settings<R: Runtime>(app: AppHandle<R>) -> AssistSettings {
    load_settings(&app)
}

#[tauri::command]
pub async fn ai_assist_save_settings<R: Runtime>(
    app: AppHandle<R>,
    settings: AssistSettings,
) -> Result<SavedSettings, String> {
    save_settings(&app, &settings).await
}

#[tauri::command]
pub async fn ai_assist_get_brief<R: Runtime>(app: AppHandle<R>) -> String {
    load_brief(&app)
}

#[tauri::command]
pub async fn ai_assist_set_brief<R: Runtime>(app: AppHandle<R>, brief: String) -> Result<(), String> {
    set_brief(&app, &brief)
}

/// What Settings shows: the built-in rules (not editable) and the full
/// system prompt these settings and the current brief produce. `settings`
/// is the unsaved form; `research` is whether the Pipecat engine's lane
/// would be on. `context_chars` and `overview_included` are the backend's
/// own count against MAX_CONTEXT_CHARS (brief included), so Settings never
/// keeps a second copy of that rule.
#[derive(Serialize)]
pub struct PromptPreview {
    built_in: String,
    full: String,
    research: Option<String>,
    context_chars: usize,
    overview_included: bool,
    brief_chars: usize,
}

#[tauri::command]
pub async fn ai_assist_prompt_preview<R: Runtime>(app: AppHandle<R>, settings: AssistSettings) -> PromptPreview {
    let brief = load_brief(&app);
    let research = research_wanted(&settings);
    let parts = prompt_parts(&settings, &brief, research);
    let brief_chars = brief.chars().count();
    PromptPreview {
        built_in: cards::built_in_rules(&parts),
        full: cards::system_prompt(&parts),
        research: research.then(|| cards::research_prompt(&prompt_parts(&settings, &brief, true))),
        context_chars: settings.context_chars(brief_chars),
        overview_included: settings.overview_included(brief_chars),
        brief_chars,
    }
}

/// An earlier meeting the brief can draw on.
#[derive(Serialize)]
pub struct EarlierMeeting {
    id: String,
    title: String,
    created_at: String,
    /// The summary's opening, plain text; empty when it has no summary yet.
    summary: String,
}

/// Most characters of one earlier meeting's summary put in the brief.
const EARLIER_SUMMARY_CHARS: usize = 1_500;

/// Earlier meetings for the brief: every word of `query` in the title or the
/// summary, newest first. Meetily keeps no attendee list, and a recording's
/// title is a timestamp until its summary names it, so the match is on
/// words (an account, a person, a topic), not on this call's title.
#[tauri::command]
pub async fn ai_assist_find_meetings(
    state: tauri::State<'_, AppState>,
    query: String,
) -> Result<Vec<EarlierMeeting>, String> {
    let words: Vec<String> = query.split_whitespace().take(6).map(|w| w.to_lowercase()).collect();
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT m.id, m.title, m.created_at, sp.result FROM meetings m \
         LEFT JOIN summary_processes sp ON sp.meeting_id = m.id \
         ORDER BY m.created_at DESC LIMIT 400",
    )
    .fetch_all(state.db_manager.pool())
    .await
    .map_err(|e| format!("could not read meetings: {e}"))?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, title, created_at, result)| {
            let summary = result.as_deref().map(summary_text).unwrap_or_default();
            let haystack = format!("{} {}", title.to_lowercase(), summary.to_lowercase());
            if !words.iter().all(|w| haystack.contains(w.as_str())) {
                return None;
            }
            Some(EarlierMeeting { id, title, created_at, summary: clip_chars(&summary, EARLIER_SUMMARY_CHARS) })
        })
        .take(12)
        .collect())
}

/// Plain summary text from a `summary_processes.result` JSON: its Markdown
/// (top level, or the `english_cache` copy) without the title heading.
fn summary_text(result: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(result) else { return String::new() };
    let markdown = value
        .get("markdown")
        .and_then(|v| v.as_str())
        .or_else(|| value.pointer("/english_cache/markdown").and_then(|v| v.as_str()))
        .unwrap_or("");
    markdown
        .lines()
        .filter(|l| !l.trim_start().starts_with("# "))
        .map(|l| l.replace("**", ""))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max).collect::<String>().trim_end())
}

/// The built-in Pipecat and Daily docs servers (public, no headers).
fn pipecat_daily_docs_servers() -> [settings::McpServer; 2] {
    let server = |id: &str, name: &str, url: &str, tool: &str| settings::McpServer {
        id: id.into(),
        name: name.into(),
        enabled: true,
        transport: McpTransport::Http,
        command: String::new(),
        args: Vec::new(),
        env: Default::default(),
        url: url.into(),
        headers: Default::default(),
        allowed_tools: vec![tool.into()],
    };
    [
        server("builtin-pipecat-docs", settings::BUILTIN_PIPECAT_DOCS_NAME, PIPECAT_DOCS_MCP_URL, PIPECAT_DOCS_TOOL),
        server("builtin-daily-docs", settings::BUILTIN_DAILY_DOCS_NAME, DAILY_DOCS_MCP_URL, DAILY_DOCS_TOOL),
    ]
}

/// Test the built-in Pipecat and Daily docs servers: each must list its
/// search tool. Returns the tools found, prefixed by server.
#[tauri::command]
pub async fn ai_assist_test_builtin_pipecat_daily() -> Result<Vec<mcp_probe::ToolInfo>, String> {
    let mut found = Vec::new();
    for server in pipecat_daily_docs_servers() {
        let tools = mcp_probe::list_tools(&server).await.map_err(|e| format!("{}: {e}", server.name))?;
        let wanted = &server.allowed_tools[0];
        if !tools.iter().any(|t| &t.name == wanted) {
            return Err(format!(
                "{} connected but does not list {wanted}, the tool lookups use (it lists {})",
                server.name,
                if tools.is_empty() { "no tools".to_string() } else { tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ") }
            ));
        }
        found.extend(tools.into_iter().filter(|t| &t.name == wanted).map(|t| mcp_probe::ToolInfo {
            name: format!("{}: {}", server.name, t.name),
            description: t.description,
        }));
    }
    Ok(found)
}

/// Test the built-in Deepgram docs server with the saved Deepgram key (the
/// key stays in Rust; the UI never sees it).
#[tauri::command]
pub async fn ai_assist_test_builtin_docs<R: Runtime>(app: AppHandle<R>) -> Result<Vec<mcp_probe::ToolInfo>, String> {
    // One read of the key, used for both the request and the redaction.
    let key = deepgram_key(&app).await.ok_or("no Deepgram API key is saved (Settings › Transcription › Deepgram)")?;
    let server = builtin_docs_server_with(&key);
    // An error body could echo the request's headers: never hand the key to the UI.
    let redact = |e: String| e.replace(key.trim(), "***");
    let tools = mcp_probe::list_tools(&server).await.map_err(redact)?;
    if !tools.iter().any(|t| t.name == DEEPGRAM_DOCS_TOOL) {
        return Err(format!(
            "connected, but the server does not list {DEEPGRAM_DOCS_TOOL}, the tool lookups use (it lists {})",
            if tools.is_empty() { "no tools".to_string() } else { tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ") }
        ));
    }
    Ok(tools)
}

/// The built-in Deepgram knowledge text, for the Knowledge section.
#[tauri::command]
pub async fn ai_assist_builtin_knowledge() -> String {
    DEEPGRAM_PUBLIC.to_string()
}

/// Connect to one MCP server and list its tools (Settings › AI Assist
/// "Test"). The same check the research lane's first lookup would hit.
#[tauri::command]
pub async fn ai_assist_test_mcp(server: settings::McpServer) -> Result<Vec<mcp_probe::ToolInfo>, String> {
    mcp_probe::list_tools(&server).await
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
            origin: None,
        }
    }

    /// A `set_brief` stand-in: the store write under `CONTEXT_SAVE`.
    fn save_brief_under_lock(store: &std::sync::Arc<Mutex<String>>, brief: &str) {
        let _guard = CONTEXT_SAVE.lock().unwrap();
        *store.lock().unwrap() = brief.to_string();
    }

    /// Runs `body` with a second thread that saves brief "B" as soon as
    /// `go` is signalled; returns after both finish.
    fn race_a_save(
        store: &std::sync::Arc<Mutex<String>>,
        body: impl FnOnce(&std::sync::mpsc::Sender<()>),
    ) {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let saver = {
            let store = store.clone();
            std::thread::spawn(move || {
                wait.recv().unwrap();
                save_brief_under_lock(&store, "B");
            })
        };
        body(&go);
        saver.join().unwrap();
    }

    #[test]
    fn a_brief_saved_while_stop_runs_is_not_taken_for_the_stopped_calls() {
        let _serial = LOCK_TESTS.lock().unwrap();
        let store = std::sync::Arc::new(Mutex::new("A".to_string()));
        let mut read = None;
        race_a_save(&store, |go| {
            read = stop_then_read(
                || {
                    go.send(()).unwrap();
                    // Give the save every chance to land before the read.
                    std::thread::sleep(Duration::from_millis(150));
                    Some(())
                },
                || store.lock().unwrap().clone(),
            );
        });
        assert_eq!(read.map(|(_, b)| b).as_deref(), Some("A"), "Stop read the next call's brief");
        assert_eq!(*store.lock().unwrap(), "B", "the next call's brief is still saved");
    }

    #[test]
    fn a_brief_saved_while_the_old_one_is_cleared_survives() {
        let _serial = LOCK_TESTS.lock().unwrap();
        let store = std::sync::Arc::new(Mutex::new("A".to_string()));
        let mut cleared = false;
        race_a_save(&store, |go| {
            cleared = clear_if_still(
                "A",
                || {
                    let current = store.lock().unwrap().clone();
                    go.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(150));
                    Some(current)
                },
                || store.lock().unwrap().clear(),
            );
        });
        assert!(cleared);
        assert_eq!(*store.lock().unwrap(), "B", "the clear deleted a brief saved for the next call");
    }

    /// The two lock tests share the global CONTEXT_SAVE: run them one at a time.
    static LOCK_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn the_builtin_overview_is_in_the_prompt_unless_turned_off() {
        let mut settings = AssistSettings::default();
        let on = compose_prompt(&settings, "", false);
        assert!(on.contains("<knowledge name=\"Deepgram, Pipecat and Daily public overview (built in)\">"));
        assert!(on.contains("# Pipecat") && on.contains("# Daily"), "the Pipecat and Daily sections are bundled");
        assert!(on.contains("Nova-3"), "the bundled text itself is sent");
        assert!(!DEEPGRAM_PUBLIC.contains("]("), "links are stripped from the bundled text");
        settings.builtin.deepgram_overview = false;
        assert!(!compose_prompt(&settings, "", false).contains("public overview (built in)"));
    }

    #[test]
    fn research_is_wanted_with_only_the_builtin_docs_server() {
        let mut settings = AssistSettings::default();
        assert!(research_wanted(&settings), "default: research on with the built-in docs");
        settings.builtin.deepgram_docs = false;
        assert!(research_wanted(&settings), "the Pipecat and Daily docs are still on");
        settings.builtin.pipecat_daily_docs = false;
        assert!(!research_wanted(&settings), "no server at all");
        settings.builtin.deepgram_docs = true;
        settings.research.enabled = false;
        assert!(!research_wanted(&settings));
        settings.research.enabled = true;
        settings.card_kinds.answer = false;
        settings.card_kinds.fact = false;
        assert!(!research_wanted(&settings), "lookups answer as answer or fact cards");
    }

    #[test]
    fn pins_round_trip_through_the_meeting_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_pins(dir.path()), Ok(vec![]), "no file: no pins");
        write_pins(dir.path(), &[card("a"), card("b")], None).unwrap();
        assert_eq!(read_pins(dir.path()).unwrap(), vec![card("a"), card("b")]);
        write_pins(dir.path(), &[], None).unwrap();
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
        let t0 = std::time::Instant::now();
        engine.on_line(2, Side::Them, "and the SLA?", t0);
        let raw = vec![super::cards::RawCard { kind: CardKind::Ask, text: "Which SLA?".into(), quote: String::new() }];
        engine.on_external_cards(9, raw, None, 1);
        let order_before: Vec<String> = engine.cards().iter().map(|c| c.id.clone()).collect();
        assert_eq!(order_before.last(), Some(&id), "the card to pin is not on top");
        assert!(pin_and_save(&mut engine, None, &id, true).is_err(), "no meeting folder");
        let order_after: Vec<String> = engine.cards().iter().map(|c| c.id.clone()).collect();
        assert_eq!(order_after, order_before, "a failed pin puts the card back where it was");
        assert!(engine.pinned().is_empty());
        let missing = std::path::Path::new("/nonexistent/w524/folder");
        assert!(pin_and_save(&mut engine, Some(missing), &id, true).is_err(), "write failure");
        assert!(engine.pinned().is_empty());
        assert_eq!(pin_and_save(&mut engine, None, "nope", true), Err("card not found".to_string()));
    }

    #[test]
    fn no_pins_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        write_pins(dir.path(), &[], None).unwrap();
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
