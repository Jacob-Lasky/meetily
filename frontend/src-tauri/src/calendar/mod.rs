//! Calendar: record meetings by themselves and have their brief written
//! beforehand (WORK-677).
//!
//! Meetily reads Me's calendar through an MCP server (for Jake: Google
//! Calendar through `mcp-connector serve google_calendar`, the work machines'
//! shared Google grant), every `REFRESH_EVERY`, for the next day. A
//! scheduler ticking every `TICK`, inside the app, then:
//! - writes each qualifying meeting's brief `prep_lead_minutes` before it
//!   starts (`prep.rs`: calendar event, earlier Meetily meetings, a
//!   knowledge lookup; one completion with the AI Assist model). Only
//!   qualifying meetings cost a model call, and nothing runs outside the app;
//! - starts a recording when a qualifying meeting starts, through the
//!   frontend's own start path (`tray::request_start`), and shows a
//!   notification with a Stop button;
//! - stops a recording it started once the meeting is over and no one has
//!   spoken for `stop_after_silence_minutes`, or after `EARLY_END_SILENCE`
//!   of silence before the end (the call ended early).
//!
//! Prepared briefs are kept per calendar event id. When any recording starts
//! during a meeting that has one, it goes into the brief box, but only if the
//! box is empty: a brief Me typed always wins (`live_assist::load_prepared_brief`).
//!
//! Settings and briefs live in the `calendar.json` Tauri store. DO NOT move
//! them into the database: a migration would stop the previous AppImage
//! opening the database, and rolling back to it is the recovery path
//! (WORK-581's constraint).

pub mod events;
pub mod prep;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Listener, Manager, Runtime};
use tauri_plugin_store::StoreExt;

use crate::live_assist::mcp_probe;
use crate::live_assist::settings::{McpServer, McpTransport};
use crate::state::AppState;
use events::{qualifies, CalEvent, Rules};

const STORE: &str = "calendar.json";
const SETTINGS_KEY: &str = "settings";
const BRIEFS_KEY: &str = "briefs";

/// How often the scheduler looks at the clock.
const TICK: Duration = Duration::from_secs(15);
/// How often the calendar is read.
const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// After a failed read, the cached events are trusted this long.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);
/// How far ahead the calendar is read.
const LOOKAHEAD_HOURS: i64 = 26;
/// A meeting is started from this long before its start time...
const START_EARLY: chrono::Duration = chrono::Duration::seconds(20);
/// ...until this long after it (the app was opened late), never past its end.
const START_LATE: chrono::Duration = chrono::Duration::minutes(10);
/// A brief is loaded into the box for a recording started from this long
/// before a meeting until its end.
const BRIEF_WINDOW_BEFORE: chrono::Duration = chrono::Duration::minutes(10);
/// Silence that ends an automatic recording before the meeting's end time.
const EARLY_END_SILENCE: Duration = Duration::from_secs(15 * 60);
/// A start request older than this did not start a recording.
const PENDING_TTL: Duration = Duration::from_secs(120);
/// Prep attempts per meeting, and the wait between them.
const PREP_ATTEMPTS: u32 = 3;
const PREP_RETRY: Duration = Duration::from_secs(5 * 60);
/// One prep's completion: written minutes ahead, so it may take its time.
const PREP_TIMEOUT: Duration = Duration::from_secs(180);
const PREP_MAX_TOKENS: u32 = 4_000;
/// Prepared briefs are kept this long after their meeting ended.
const BRIEF_KEEP: chrono::Duration = chrono::Duration::days(2);

/// Emitted to the frontend: `AutoRecordEvent`.
pub const AUTO_RECORD_EVENT: &str = "calendar-auto-record";
/// Emitted when a brief was prepared (or its prep failed): `{ eventId }`.
pub const BRIEF_PREPARED_EVENT: &str = "calendar-brief-prepared";

/// The knowledge service the prep asks about a meeting's organizations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lookup {
    pub server: McpServer,
    /// Called with `{"query": "<organizations and title>"}`.
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CalendarSettings {
    /// Start a recording when a qualifying meeting starts.
    pub auto_record: bool,
    /// Write each qualifying meeting's brief before it starts.
    pub auto_prep: bool,
    /// The calendar's MCP server (it must offer `list_events`). None: no
    /// calendar, nothing runs.
    pub server: Option<McpServer>,
    /// Empty: the primary calendar.
    pub calendar_id: String,
    pub rules: Rules,
    pub prep_lead_minutes: u32,
    pub stop_after_silence_minutes: u32,
    pub lookup: Option<Lookup>,
}

impl Default for CalendarSettings {
    fn default() -> Self {
        Self {
            auto_record: false,
            auto_prep: false,
            server: None,
            calendar_id: String::new(),
            rules: Rules::default(),
            prep_lead_minutes: 30,
            stop_after_silence_minutes: 3,
            lookup: None,
        }
    }
}

impl CalendarSettings {
    fn active(&self) -> bool {
        self.server.is_some() && (self.auto_record || self.auto_prep)
    }

    pub fn validate(&self) -> Result<(), String> {
        let check = |what: &str, s: &McpServer| -> Result<(), String> {
            match s.transport {
                McpTransport::Stdio if s.command.trim().is_empty() => Err(format!("{what}: give the command to run")),
                McpTransport::Http if !s.url.trim().starts_with("http") => Err(format!("{what}: give an http(s) URL")),
                McpTransport::Sse => Err(format!("{what}: only a local command or a streamable-HTTP URL works here")),
                _ => Ok(()),
            }
        };
        if let Some(s) = &self.server {
            check("Calendar server", s)?;
        }
        if let Some(l) = &self.lookup {
            check("Lookup server", &l.server)?;
            if l.tool.trim().is_empty() {
                return Err("Lookup: name the tool to call".into());
            }
        }
        if !(5..=240).contains(&self.prep_lead_minutes) {
            return Err("Prepare the brief between 5 and 240 minutes ahead".into());
        }
        if !(1..=30).contains(&self.stop_after_silence_minutes) {
            return Err("Stop after 1 to 30 minutes of silence".into());
        }
        if (self.auto_record || self.auto_prep) && self.server.is_none() {
            return Err("Set the calendar server first".into());
        }
        Ok(())
    }
}

/// A brief written for one calendar event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedBrief {
    pub event_id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub text: String,
    pub prepared_at: DateTime<Utc>,
    /// What fed it, e.g. ["calendar", "2 earlier meetings", "DeepHive"].
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoRecordEvent {
    /// started, stopped, skipped, failed.
    pub state: &'static str,
    pub event_id: String,
    pub title: String,
    pub message: String,
}

/// The recording the calendar started.
#[derive(Debug, Clone)]
struct AutoRun {
    event: CalEvent,
    started: Instant,
}

#[derive(Default)]
struct State {
    events: Vec<CalEvent>,
    fetched_at: Option<Instant>,
    fetch_error: Option<String>,
    /// Events whose start the scheduler has acted on (started or skipped).
    handled: HashSet<String>,
    prepping: HashSet<String>,
    prep_failures: HashMap<String, (u32, Instant, String)>,
    /// Asked the frontend to start a recording for this event.
    pending: Option<(CalEvent, Instant)>,
    auto: Option<AutoRun>,
    last_speech: Option<Instant>,
    refresh_now: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

// ============================================================================
// STORE
// ============================================================================

pub fn load_settings<R: Runtime>(app: &AppHandle<R>) -> CalendarSettings {
    app.store(STORE)
        .ok()
        .and_then(|s| s.get(SETTINGS_KEY))
        .and_then(|v| match serde_json::from_value(v) {
            Ok(settings) => Some(settings),
            Err(e) => {
                warn!("calendar: settings unreadable, using defaults: {e}");
                None
            }
        })
        .unwrap_or_default()
}

fn save_settings<R: Runtime>(app: &AppHandle<R>, settings: &CalendarSettings) -> Result<(), String> {
    settings.validate()?;
    let store = app.store(STORE).map_err(|e| format!("calendar store unavailable: {e}"))?;
    store.set(SETTINGS_KEY, serde_json::to_value(settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("could not save the calendar settings: {e}"))
}

fn load_briefs<R: Runtime>(app: &AppHandle<R>) -> BTreeMap<String, PreparedBrief> {
    app.store(STORE)
        .ok()
        .and_then(|s| s.get(BRIEFS_KEY))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Held while the briefs map is read, changed and written back.
static BRIEFS_SAVE: Mutex<()> = Mutex::new(());

fn save_brief<R: Runtime>(app: &AppHandle<R>, brief: PreparedBrief) -> Result<(), String> {
    let _guard = BRIEFS_SAVE.lock().unwrap();
    let mut briefs = load_briefs(app);
    let cutoff = Utc::now() - BRIEF_KEEP;
    briefs.retain(|_, b| b.end > cutoff);
    briefs.insert(brief.event_id.clone(), brief);
    let store = app.store(STORE).map_err(|e| format!("calendar store unavailable: {e}"))?;
    store.set(BRIEFS_KEY, serde_json::to_value(&briefs).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("could not save the prepared brief: {e}"))
}

// ============================================================================
// READING THE CALENDAR
// ============================================================================

/// Events from an hour ago to `LOOKAHEAD_HOURS` ahead, in start order.
async fn fetch_events(settings: &CalendarSettings) -> Result<Vec<CalEvent>, String> {
    let server = settings.server.as_ref().ok_or("no calendar server is set")?;
    // The server reads a time with no offset in the zone it is given (its
    // tool description asks for local time plus an IANA `timeZone`).
    let now = chrono::Local::now();
    let mut args = serde_json::json!({
        "startTime": (now - chrono::Duration::hours(1)).format("%Y-%m-%dT%H:%M:%S").to_string(),
        "endTime": (now + chrono::Duration::hours(LOOKAHEAD_HOURS)).format("%Y-%m-%dT%H:%M:%S").to_string(),
        "orderBy": "startTime",
        "pageSize": 100,
    });
    if let Ok(tz) = iana_time_zone::get_timezone() {
        args["timeZone"] = serde_json::Value::String(tz);
    }
    if !settings.calendar_id.trim().is_empty() {
        args["calendarId"] = serde_json::Value::String(settings.calendar_id.trim().to_string());
    }
    let mut all = Vec::new();
    for _page in 0..5 {
        let result = mcp_probe::call_tool(server, "list_events", args.clone()).await?;
        let (events, next) = events::parse_events(&mcp_probe::result_text(&result))?;
        all.extend(events);
        match next {
            Some(token) => args["pageToken"] = serde_json::Value::String(token),
            None => break,
        }
    }
    all.sort_by_key(|e| e.start);
    Ok(all)
}

async fn refresh<R: Runtime>(app: &AppHandle<R>, settings: &CalendarSettings) {
    match fetch_events(settings).await {
        Ok(list) => {
            let qualifying = list.iter().filter(|e| qualifies(e, &settings.rules).is_ok()).count();
            info!("calendar: read {} event(s), {qualifying} qualifying", list.len());
            let mut st = STATE.lock().unwrap();
            st.events = list;
            st.fetched_at = Some(Instant::now());
            st.fetch_error = None;
        }
        Err(e) => {
            warn!("calendar: could not read the calendar: {e}");
            let mut st = STATE.lock().unwrap();
            st.fetch_error = Some(e);
            // Keep retrying on the normal cadence, from now.
            if st.fetched_at.is_some_and(|t| t.elapsed() > STALE_AFTER) {
                st.events.clear();
                st.fetched_at = None;
            }
        }
    }
    let _ = app.emit(BRIEF_PREPARED_EVENT, serde_json::json!({ "eventId": null }));
}

// ============================================================================
// SCHEDULER
// ============================================================================

/// Start the scheduler and the transcript listener that times silence.
/// Called once from the app's setup.
pub fn start<R: Runtime>(app: &AppHandle<R>) {
    app.listen("transcript-update", |event: tauri::Event| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(event.payload()) else { return };
        let partial = v.get("is_partial").and_then(|p| p.as_bool()).unwrap_or(false);
        let spoken = v.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.trim().is_empty());
        if !partial && spoken {
            STATE.lock().unwrap().last_speech = Some(Instant::now());
        }
    });
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // The database (earlier meetings, the model setting) comes up after setup.
        while app.try_state::<AppState>().is_none() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        info!("calendar: scheduler started");
        loop {
            tick(&app).await;
            tokio::time::sleep(TICK).await;
        }
    });
}

async fn tick<R: Runtime>(app: &AppHandle<R>) {
    let settings = load_settings(app);
    let now = Utc::now();
    let recording = crate::audio::recording_commands::is_recording().await;
    // A recording the calendar started is stopped by it even after both
    // switches were turned off mid-meeting.
    if recording {
        auto_stop_check(app, &settings, now);
    }
    if !settings.active() {
        return;
    }
    // Start and prep run on the events already read, before any refresh: a
    // slow calendar read must not delay a meeting's start.
    let briefs = load_briefs(app);
    let events: Vec<CalEvent> = STATE.lock().unwrap().events.clone();
    // A start asked for but not begun yet counts as recording, so a second
    // meeting due in the same moment is not started over it.
    let mut busy = recording || STATE.lock().unwrap().pending.as_ref().is_some_and(|(_, at)| at.elapsed() < PENDING_TTL);
    for ev in events.iter().filter(|e| qualifies(e, &settings.rules).is_ok()) {
        if settings.auto_prep && prep_due(ev, now, &settings, briefs.contains_key(&ev.id)) {
            spawn_prep(app, ev.clone(), false);
        }
        if settings.auto_record && start_due(ev, now) {
            // Keyed by start time too: a meeting moved after its start was
            // handled is started again at its new time.
            let first = STATE.lock().unwrap().handled.insert(handled_key(ev));
            if first {
                begin(app, ev.clone(), busy).await;
                busy = true;
            }
        }
    }
    let due = {
        let mut st = STATE.lock().unwrap();
        let due = st.refresh_now || st.fetched_at.is_none_or(|t| t.elapsed() >= REFRESH_EVERY);
        st.refresh_now = false;
        due
    };
    if due {
        refresh(app, &settings).await;
    }
}

fn handled_key(ev: &CalEvent) -> String {
    format!("{}@{}", ev.id, ev.start.timestamp())
}

fn prep_due(ev: &CalEvent, now: DateTime<Utc>, settings: &CalendarSettings, have_brief: bool) -> bool {
    if have_brief || ev.end <= now || ev.start - now > chrono::Duration::minutes(settings.prep_lead_minutes as i64) {
        return false;
    }
    let st = STATE.lock().unwrap();
    if st.prepping.contains(&ev.id) {
        return false;
    }
    match st.prep_failures.get(&ev.id) {
        Some((n, at, _)) => *n < PREP_ATTEMPTS && at.elapsed() >= PREP_RETRY,
        None => true,
    }
}

fn start_due(ev: &CalEvent, now: DateTime<Utc>) -> bool {
    now >= ev.start - START_EARLY && now < (ev.start + START_LATE).min(ev.end)
}

/// A qualifying meeting is starting.
async fn begin<R: Runtime>(app: &AppHandle<R>, ev: CalEvent, recording: bool) {
    if !recording {
        start_recording(app, ev);
        return;
    }
    // Back-to-back: the recording the calendar started for the meeting that
    // has just ended is closed, and this one starts on its own.
    let previous = STATE.lock().unwrap().auto.clone();
    match previous {
        Some(run) if run.event.id != ev.id && Utc::now() >= run.event.end - chrono::Duration::minutes(1) => {
            info!("calendar: \"{}\" is starting; stopping the recording of \"{}\"", ev.title, run.event.title);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                stop_auto(&app, "the next meeting is starting");
                // Wait for the stop (save included) to finish before starting.
                for _ in 0..120 {
                    if !crate::audio::recording_commands::is_recording().await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                if crate::audio::recording_commands::is_recording().await {
                    announce(&app, "skipped", &ev, "the previous recording did not stop in time".into());
                } else {
                    start_recording(&app, ev);
                }
            });
        }
        _ => {
            info!("calendar: \"{}\" is starting but Meetily is already recording; leaving that recording alone", ev.title);
            announce(app, "skipped", &ev, "Meetily was already recording, so this meeting was not started separately".into());
        }
    }
}

fn start_recording<R: Runtime>(app: &AppHandle<R>, ev: CalEvent) {
    info!("calendar: starting the recording for \"{}\"", ev.title);
    STATE.lock().unwrap().pending = Some((ev.clone(), Instant::now()));
    if !crate::tray::request_start(app, Some(&ev.title)) {
        STATE.lock().unwrap().pending = None;
        announce(app, "failed", &ev, "Meetily's window is not open, so the recording could not start".into());
    }
}

/// Stop an automatic recording that is over.
fn auto_stop_check<R: Runtime>(app: &AppHandle<R>, settings: &CalendarSettings, now: DateTime<Utc>) {
    let (run, silence) = {
        let st = STATE.lock().unwrap();
        let Some(run) = st.auto.clone() else { return };
        let since = st.last_speech.unwrap_or(run.started);
        (run, since.elapsed())
    };
    let after_end = now >= run.event.end;
    let quiet_after_end = Duration::from_secs(settings.stop_after_silence_minutes as u64 * 60);
    let reason = if after_end && silence >= quiet_after_end {
        format!("the meeting ended and no one has spoken for {} min", silence.as_secs() / 60)
    } else if !after_end && silence >= EARLY_END_SILENCE && run.started.elapsed() >= EARLY_END_SILENCE {
        format!("no one has spoken for {} min", silence.as_secs() / 60)
    } else {
        return;
    };
    stop_auto(app, &reason);
}

/// Stop the recording the calendar started, if there is one.
fn stop_auto<R: Runtime>(app: &AppHandle<R>, reason: &str) -> bool {
    let Some(run) = STATE.lock().unwrap().auto.take() else { return false };
    info!("calendar: stopping the recording of \"{}\": {reason}", run.event.title);
    crate::tray::request_stop(app, false);
    announce(app, "stopped", &run.event, format!("Stopped: {reason}"));
    true
}

// ============================================================================
// RECORDING HOOKS (called by recording_commands)
// ============================================================================

/// A recording is starting (any start: button, tray or calendar). Notes
/// whether the calendar asked for it, and loads the brief prepared for the
/// meeting now on into an empty brief box.
pub fn on_recording_start<R: Runtime>(app: &AppHandle<R>) {
    let now = Utc::now();
    let (auto_event, events) = {
        let mut st = STATE.lock().unwrap();
        st.last_speech = None;
        let pending = st.pending.take().filter(|(_, at)| at.elapsed() < PENDING_TTL).map(|(ev, _)| ev);
        st.auto = pending.clone().map(|event| AutoRun { event, started: Instant::now() });
        (pending, st.events.clone())
    };
    let briefs = load_briefs(app);
    let brief = match &auto_event {
        Some(ev) => briefs.get(&ev.id),
        None => brief_for_now(&events, &briefs, now),
    };
    if let Some(brief) = brief {
        match crate::live_assist::load_prepared_brief(app, &brief.text) {
            Ok(true) => info!("calendar: loaded the brief prepared for \"{}\"", brief.title),
            Ok(false) => info!("calendar: a brief was already in the box; the one prepared for \"{}\" was not loaded", brief.title),
            Err(e) => warn!("calendar: could not load the prepared brief: {e}"),
        }
    }
    if let Some(ev) = auto_event {
        announce(app, "started", &ev, "Recording started from your calendar".into());
        notify_started(app, &ev);
    }
}

/// The brief of the meeting on now (from `BRIEF_WINDOW_BEFORE` before its
/// start to its end); with several, the one whose start is nearest.
fn brief_for_now<'a>(
    events: &[CalEvent],
    briefs: &'a BTreeMap<String, PreparedBrief>,
    now: DateTime<Utc>,
) -> Option<&'a PreparedBrief> {
    let in_window = |start: DateTime<Utc>, end: DateTime<Utc>| now >= start - BRIEF_WINDOW_BEFORE && now < end;
    // The cached events can be stale (a moved meeting); the brief's own times
    // are the prep-time copy. Prefer the live times when the event is known.
    briefs
        .values()
        .filter(|b| match events.iter().find(|e| e.id == b.event_id) {
            Some(e) => in_window(e.start, e.end),
            None => in_window(b.start, b.end),
        })
        .min_by_key(|b| (b.start - now).num_seconds().abs())
}

/// The recording is stopping: whatever started it, the calendar no longer
/// stops it.
pub fn on_recording_stop() {
    STATE.lock().unwrap().auto = None;
}

// ============================================================================
// NOTICES
// ============================================================================

fn announce<R: Runtime>(app: &AppHandle<R>, state: &'static str, ev: &CalEvent, message: String) {
    let payload = AutoRecordEvent { state, event_id: ev.id.clone(), title: ev.title.clone(), message };
    if let Err(e) = app.emit(AUTO_RECORD_EVENT, &payload) {
        error!("calendar: failed to emit {AUTO_RECORD_EVENT}: {e}");
    }
}

/// A desktop notification for a recording the calendar started, with a
/// "Stop recording" button (Linux: notify-rust over D-Bus; tauri's
/// notification plugin has no buttons on desktop). Elsewhere a plain
/// notification; the in-app toast carries the button there.
fn notify_started<R: Runtime>(app: &AppHandle<R>, ev: &CalEvent) {
    let title = ev.title.clone();
    let event_id = ev.id.clone();
    #[cfg(target_os = "linux")]
    {
        let app = app.clone();
        std::thread::spawn(move || {
            let shown = notify_rust::Notification::new()
                .appname("Meetily")
                .summary("Meetily is recording")
                .body(&format!("\u{201c}{title}\u{201d} started from your calendar."))
                .icon("meetily")
                .action("stop", "Stop recording")
                .timeout(notify_rust::Timeout::Milliseconds(120_000))
                .show();
            match shown {
                Ok(handle) => handle.wait_for_action(|action| {
                    if action == "stop" {
                        cancel(&app, &event_id);
                    }
                }),
                Err(e) => warn!("calendar: could not show the notification: {e}"),
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        use tauri_plugin_notification::NotificationExt;
        let _ = event_id;
        if let Err(e) = app
            .notification()
            .builder()
            .title("Meetily is recording")
            .body(format!("\u{201c}{title}\u{201d} started from your calendar."))
            .show()
        {
            warn!("calendar: could not show the notification: {e}");
        }
    }
}

/// Me pressed Stop on the notification or the toast: stop the automatic
/// recording of `event_id` (what was recorded so far is kept, like any stop).
fn cancel<R: Runtime>(app: &AppHandle<R>, event_id: &str) -> bool {
    let current = STATE.lock().unwrap().auto.as_ref().map(|r| r.event.id.clone());
    if current.as_deref() != Some(event_id) {
        return false;
    }
    stop_auto(app, "you stopped it")
}

// ============================================================================
// PREP
// ============================================================================

/// Prepare `ev`'s brief in the background (`force`: even if one exists).
fn spawn_prep<R: Runtime>(app: &AppHandle<R>, ev: CalEvent, force: bool) {
    if !STATE.lock().unwrap().prepping.insert(ev.id.clone()) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = prepare(&app, &ev, force).await;
        let mut st = STATE.lock().unwrap();
        st.prepping.remove(&ev.id);
        match result {
            Ok(_) => {
                st.prep_failures.remove(&ev.id);
            }
            Err(e) => {
                warn!("calendar: could not prepare the brief for \"{}\": {e}", ev.title);
                let n = st.prep_failures.get(&ev.id).map(|f| f.0).unwrap_or(0) + 1;
                st.prep_failures.insert(ev.id.clone(), (n, Instant::now(), e));
            }
        }
        drop(st);
        let _ = app.emit(BRIEF_PREPARED_EVENT, serde_json::json!({ "eventId": ev.id }));
    });
}

/// Write and store `ev`'s brief.
async fn prepare<R: Runtime>(app: &AppHandle<R>, ev: &CalEvent, force: bool) -> Result<PreparedBrief, String> {
    if !force {
        if let Some(b) = load_briefs(app).get(&ev.id) {
            return Ok(b.clone());
        }
    }
    let started = Instant::now();
    let settings = load_settings(app);
    let state = app.try_state::<AppState>().ok_or("database not ready")?;
    let meetings = crate::live_assist::recent_meetings(state.db_manager.pool()).await?;
    let earlier = prep::pick_earlier(meetings, ev, prep::MAX_EARLIER);
    let mut sources = vec!["calendar".to_string()];
    if !earlier.is_empty() {
        sources.push(format!("{} earlier meeting{}", earlier.len(), if earlier.len() == 1 { "" } else { "s" }));
    }
    let mut knowledge: Option<(String, String)> = None;
    if let (Some(lookup), Some(query)) = (&settings.lookup, prep::knowledge_query(ev)) {
        match mcp_probe::call_tool(&lookup.server, lookup.tool.trim(), serde_json::json!({ "query": query })).await {
            Ok(result) => {
                let text = mcp_probe::result_text(&result);
                if !text.trim().is_empty() {
                    sources.push(lookup.server.name.clone());
                    knowledge = Some((lookup.server.name.clone(), text));
                }
            }
            // The brief is still worth writing from the calendar and earlier meetings.
            Err(e) => warn!("calendar: the {} lookup failed: {e}", lookup.server.name),
        }
    }
    let prompt = prep::user_prompt(ev, &earlier, knowledge.as_ref().map(|(s, t)| (s.as_str(), t.as_str())));
    let body = crate::live_assist::complete_with_assist_model(app, prep::SYSTEM_PROMPT, &prompt, PREP_MAX_TOKENS, PREP_TIMEOUT).await?;
    if body.trim().is_empty() {
        return Err("the model returned an empty brief".into());
    }
    let brief = PreparedBrief {
        event_id: ev.id.clone(),
        title: ev.title.clone(),
        start: ev.start,
        end: ev.end,
        text: prep::brief_text(ev, &body),
        prepared_at: Utc::now(),
        sources,
    };
    save_brief(app, brief.clone())?;
    info!(
        "calendar: prepared the brief for \"{}\" in {} ms ({} chars; {})",
        ev.title,
        started.elapsed().as_millis(),
        brief.text.chars().count(),
        brief.sources.join(", ")
    );
    Ok(brief)
}

// ============================================================================
// COMMANDS
// ============================================================================

#[tauri::command]
pub async fn calendar_get_settings<R: Runtime>(app: AppHandle<R>) -> CalendarSettings {
    load_settings(&app)
}

#[tauri::command]
pub async fn calendar_save_settings<R: Runtime>(app: AppHandle<R>, settings: CalendarSettings) -> Result<(), String> {
    save_settings(&app, &settings)?;
    info!(
        "calendar: settings saved (auto-record {}, auto-prep {})",
        settings.auto_record, settings.auto_prep
    );
    STATE.lock().unwrap().refresh_now = true;
    Ok(())
}

/// One upcoming event as Settings lists it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Upcoming {
    id: String,
    title: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    /// None: it qualifies; else why not.
    skip_reason: Option<String>,
    interview: bool,
    /// none, preparing, ready, failed.
    brief_state: &'static str,
    brief_error: Option<String>,
    brief: Option<PreparedBrief>,
    /// The scheduler already acted on its start.
    handled: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpcomingList {
    events: Vec<Upcoming>,
    /// Seconds since the calendar was last read.
    fetched_secs_ago: Option<u64>,
    error: Option<String>,
}

/// The next day's events and what the scheduler makes of each. `settings`:
/// the unsaved form (read the calendar now with its server and rules, as a
/// test); None: the saved settings and the scheduler's last read, re-read
/// first when `refresh`.
#[tauri::command]
pub async fn calendar_upcoming<R: Runtime>(
    app: AppHandle<R>,
    settings: Option<CalendarSettings>,
    refresh_now: bool,
) -> Result<UpcomingList, String> {
    let (rules, events, fetched, error) = match settings {
        Some(form) => {
            if form.server.is_none() {
                return Err("Set the calendar server first".into());
            }
            let events = fetch_events(&form).await?;
            (form.rules, events, Some(0), None)
        }
        None => {
            let saved = load_settings(&app);
            if saved.server.is_none() {
                return Err("No calendar server is set".into());
            }
            if refresh_now {
                refresh(&app, &saved).await;
            }
            let st = STATE.lock().unwrap();
            (saved.rules, st.events.clone(), st.fetched_at.map(|t| t.elapsed().as_secs()), st.fetch_error.clone())
        }
    };
    let now = Utc::now();
    let briefs = load_briefs(&app);
    let st = STATE.lock().unwrap();
    let events = events
        .into_iter()
        .filter(|e| e.end > now)
        .map(|e| {
            let brief = briefs.get(&e.id).cloned();
            let failure = st.prep_failures.get(&e.id);
            let brief_state = if st.prepping.contains(&e.id) {
                "preparing"
            } else if brief.is_some() {
                "ready"
            } else if failure.is_some() {
                "failed"
            } else {
                "none"
            };
            Upcoming {
                skip_reason: qualifies(&e, &rules).err(),
                interview: events::is_interview(&e),
                brief_state,
                brief_error: failure.map(|f| f.2.clone()),
                brief,
                handled: st.handled.contains(&handled_key(&e)),
                id: e.id,
                title: e.title,
                start: e.start,
                end: e.end,
            }
        })
        .collect();
    Ok(UpcomingList { events, fetched_secs_ago: fetched, error })
}

/// Prepare one event's brief now, through the same path the scheduler uses
/// (re-writing an existing one). Waits for it and returns it.
#[tauri::command]
pub async fn calendar_prepare_now<R: Runtime>(app: AppHandle<R>, event_id: String) -> Result<PreparedBrief, String> {
    let ev = STATE
        .lock()
        .unwrap()
        .events
        .iter()
        .find(|e| e.id == event_id)
        .cloned()
        .ok_or("that event is not in the calendar Meetily last read; refresh first")?;
    if !STATE.lock().unwrap().prepping.insert(ev.id.clone()) {
        return Err("its brief is already being prepared".into());
    }
    let result = prepare(&app, &ev, true).await;
    {
        let mut st = STATE.lock().unwrap();
        st.prepping.remove(&ev.id);
        match &result {
            Ok(_) => {
                st.prep_failures.remove(&ev.id);
            }
            Err(e) => {
                let n = st.prep_failures.get(&ev.id).map(|f| f.0).unwrap_or(0) + 1;
                st.prep_failures.insert(ev.id.clone(), (n, Instant::now(), e.clone()));
            }
        }
    }
    let _ = app.emit(BRIEF_PREPARED_EVENT, serde_json::json!({ "eventId": ev.id }));
    result
}

/// Stop the recording the calendar started for `event_id` (the toast's and
/// the notification's Stop button).
#[tauri::command]
pub async fn calendar_cancel_auto_record<R: Runtime>(app: AppHandle<R>, event_id: String) -> Result<(), String> {
    if cancel(&app, &event_id) {
        Ok(())
    } else {
        Err("That recording is no longer the one the calendar started".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, start_min: i64, len_min: i64, now: DateTime<Utc>) -> CalEvent {
        let v = serde_json::json!({"events": [{
            "id": id, "summary": id,
            "start": {"dateTime": (now + chrono::Duration::minutes(start_min)).to_rfc3339()},
            "end": {"dateTime": (now + chrono::Duration::minutes(start_min + len_min)).to_rfc3339()},
        }]});
        events::parse_events(&v.to_string()).unwrap().0.remove(0)
    }

    fn brief(e: &CalEvent) -> PreparedBrief {
        PreparedBrief {
            event_id: e.id.clone(),
            title: e.title.clone(),
            start: e.start,
            end: e.end,
            text: format!("brief for {}", e.id),
            prepared_at: e.start,
            sources: vec![],
        }
    }

    #[test]
    fn a_meeting_starts_from_just_before_until_ten_minutes_in() {
        let now = Utc::now();
        assert!(!start_due(&ev("a", 1, 30, now), now), "a minute early is too early");
        assert!(start_due(&ev("a", 0, 30, now), now));
        assert!(start_due(&ev("a", -9, 30, now), now), "opened late: still starts");
        assert!(!start_due(&ev("a", -11, 30, now), now));
        assert!(!start_due(&ev("a", -5, 4, now), now), "never past its end");
    }

    #[test]
    fn the_brief_loaded_is_the_one_for_the_meeting_on_now() {
        let now = Utc::now();
        let past = ev("past", -90, 30, now);
        let on = ev("on", -5, 30, now);
        let next = ev("next", 25, 30, now);
        let soon = ev("soon", 8, 30, now);
        let briefs: BTreeMap<String, PreparedBrief> =
            [&past, &on, &next].iter().map(|e| (e.id.clone(), brief(e))).collect();
        let events = vec![past.clone(), on.clone(), next.clone()];
        assert_eq!(brief_for_now(&events, &briefs, now).unwrap().event_id, "on");
        // Nothing on now: the meeting 25 min away is not this recording's.
        let only_next: BTreeMap<_, _> = [(next.id.clone(), brief(&next))].into();
        assert!(brief_for_now(&events, &only_next, now).is_none());
        // Starting a recording 8 min early picks up that meeting's brief.
        let with_soon: BTreeMap<_, _> = [(soon.id.clone(), brief(&soon))].into();
        assert_eq!(brief_for_now(&[soon.clone()], &with_soon, now).unwrap().event_id, "soon");
        // A meeting moved since its prep follows the calendar's new time.
        let moved = ev("on", 60, 30, now);
        assert!(brief_for_now(&[moved], &briefs, now).is_none());
    }

    #[test]
    fn prep_runs_inside_the_lead_time_once_and_retries_failures_later() {
        let now = Utc::now();
        let settings = CalendarSettings { prep_lead_minutes: 30, ..Default::default() };
        let far = ev("w677-far", 45, 30, now);
        let near = ev("w677-near", 20, 30, now);
        assert!(!prep_due(&far, now, &settings, false));
        assert!(prep_due(&near, now, &settings, false));
        assert!(!prep_due(&near, now, &settings, true), "already prepared");
        STATE.lock().unwrap().prep_failures.insert(near.id.clone(), (1, Instant::now(), "x".into()));
        assert!(!prep_due(&near, now, &settings, false), "retried only after PREP_RETRY");
        STATE.lock().unwrap().prep_failures.insert(near.id.clone(), (PREP_ATTEMPTS, Instant::now() - PREP_RETRY, "x".into()));
        assert!(!prep_due(&near, now, &settings, false), "gives up after PREP_ATTEMPTS");
        STATE.lock().unwrap().prep_failures.remove(&near.id);
    }

    #[test]
    fn settings_are_checked_before_saving() {
        let server = McpServer {
            id: "c".into(),
            name: "Google Calendar".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: "mcp-connector".into(),
            args: vec!["serve".into(), "google_calendar".into()],
            env: Default::default(),
            url: String::new(),
            headers: Default::default(),
            allowed_tools: vec![],
        };
        let ok = CalendarSettings { auto_record: true, server: Some(server.clone()), ..Default::default() };
        assert_eq!(ok.validate(), Ok(()));
        assert!(CalendarSettings { auto_record: true, ..Default::default() }.validate().is_err(), "no server");
        assert!(CalendarSettings { prep_lead_minutes: 1, ..ok.clone() }.validate().is_err());
        let blank = McpServer { command: " ".into(), ..server.clone() };
        assert!(CalendarSettings { server: Some(blank), ..ok.clone() }.validate().is_err());
        let lookup = Lookup { server, tool: String::new() };
        assert!(CalendarSettings { lookup: Some(lookup), ..ok }.validate().is_err(), "a lookup needs its tool");
        // Old or partial stores read with defaults.
        let partial: CalendarSettings = serde_json::from_value(serde_json::json!({"autoRecord": true})).unwrap();
        assert!(partial.auto_record && partial.rules.require_video && partial.prep_lead_minutes == 30);
    }
}
