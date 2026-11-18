//! Pipecat live engine (WORK-559): live transcription and live assist through
//! a Python sidecar running Pipecat, instead of straight to Deepgram.
//!
//! Settings > Transcription offers "Live engine: Direct (nova-3) | Pipecat
//! (Flux)" (store `live_engine.json`, key `engine`; Direct is the default and
//! the rollback). On the Pipecat engine:
//!
//! - Rust keeps capture and saving. Each channel's 16 kHz linear16 PCM goes to
//!   the sidecar over 127.0.0.1, one WebSocket per channel, authenticated with
//!   a random token made for this recording. The sidecar never opens an audio
//!   device.
//! - The sidecar runs Deepgram Flux per channel (`DeepgramFluxSTTService`) and
//!   answers in Flux's own `TurnInfo` shape, so `deepgram::run_realtime_core`
//!   handles it exactly as it handles Deepgram: same time mapping, trailing
//!   silence, bleed matching (Flux sends word timings) and transcript events.
//!   This replaces the direct Deepgram sockets for the recording, so the
//!   audio is not billed twice.
//! - Live assist is triggered by Flux's turn events in the sidecar, not by the
//!   Rust timer; `live_assist` bridges cards back over `/control`.
//! - If the sidecar cannot start, or fails mid-recording, the rest of the
//!   recording goes to Direct and the user sees a toast saying so
//!   (`Outcome::FallBack`). Transcription never silently stops.
//!
//! The sidecar's Python environment lives under the app data dir and is built
//! by `uv sync` ahead of time (`prepare`, run when the engine is switched on
//! and at app start). A recording never waits on a download: it execs that
//! environment's python and falls back to Direct if it is not built yet.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_store::StoreExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

use super::deepgram::{self, DeepgramOptions, RealtimeEndpoints};
use super::recording_state::AudioChunk;

const STORE: &str = "live_engine.json";
const ENGINE_KEY: &str = "engine";
/// Emitted while `prepare` runs: `{ "state": "preparing" | "ready" | "error", "message"?: string }`.
pub const PREPARE_EVENT: &str = "live-engine-prepare";

/// From spawn to the sidecar's `{"port": N}` line. Normally ~1-2 s (Pipecat's
/// imports); the environment is already built, so nothing downloads here.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// After stdin closes, how long the sidecar gets to exit before it is killed.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Flux model when the configured Deepgram model is not a Flux one.
const DEFAULT_FLUX_MODEL: &str = "flux-general-en";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveEngine {
    Direct,
    Pipecat,
}

pub fn load_engine<R: Runtime>(app: &AppHandle<R>) -> LiveEngine {
    let Ok(store) = app.store(STORE) else {
        return LiveEngine::Direct;
    };
    store
        .get(ENGINE_KEY)
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or(LiveEngine::Direct)
}

// ============================================================================
// RUN (called by deepgram::run_realtime_transcription)
// ============================================================================

pub(crate) enum Outcome {
    /// The recording ended on the Pipecat engine (with its result).
    Finished(Result<(), String>),
    /// Continue the recording on Direct with what is left of the audio:
    /// the recent audio the Pipecat engine had not finished (replayed first),
    /// then the rest of the recording. `shown` says how far each channel's
    /// lines already got, so the replay does not show them twice.
    FallBack {
        receiver: mpsc::UnboundedReceiver<AudioChunk>,
        reason: String,
        shown: deepgram::ShownUntil,
    },
}

/// Most recent audio kept for replay on a fallback. It covers an unfinished
/// turn (Flux commits a turn only at its end, so a crash loses the turn in
/// progress). Limit: a turn already longer than this when the sidecar dies
/// loses its start. The cap bounds memory and the burst replayed to Deepgram
/// (30 s was accepted live on 2026-11-18).
const REPLAY_SECS: f64 = 30.0;

pub(crate) async fn run<R: Runtime>(
    app: &AppHandle<R>,
    deepgram_api_key: &str,
    options: &DeepgramOptions,
    language: Option<String>,
    receiver: mpsc::UnboundedReceiver<AudioChunk>,
) -> Outcome {
    let fall_back = |receiver, reason| Outcome::FallBack { receiver, reason, shown: deepgram::ShownUntil::default() };
    let command = match SidecarCommand::resolve(app, false) {
        Ok(command) => command,
        Err(reason) => return fall_back(receiver, reason),
    };
    let sidecar = match Sidecar::spawn(command, deepgram_api_key, STARTUP_TIMEOUT).await {
        Ok(sidecar) => sidecar,
        Err(reason) => {
            // Most often the environment is not built yet: build it for next time.
            spawn_prepare(app.clone());
            return fall_back(receiver, reason);
        }
    };
    let generation = crate::live_assist::next_sidecar_generation();
    let control = match sidecar.connect_control(app.clone(), generation).await {
        Ok(control) => control,
        Err(reason) => {
            sidecar.shutdown().await;
            return fall_back(receiver, reason);
        }
    };
    crate::live_assist::attach_sidecar(generation, control.tx.clone());
    if control.ended.load(Ordering::SeqCst) {
        // Closed before the attach: the reader's own detach found nothing.
        crate::live_assist::detach_sidecar(generation);
    }

    let mut flux_options = options.clone();
    if !flux_options.model.starts_with("flux") {
        flux_options.model = DEFAULT_FLUX_MODEL.to_string();
    }
    info!(
        "Pipecat live engine: sidecar pid {:?} on port {}, model {}",
        sidecar.pid(),
        sidecar.port,
        flux_options.model
    );
    let (inner_tx, inner_rx) = mpsc::unbounded_channel();
    let shown = Arc::new(std::sync::Mutex::new(deepgram::ShownUntil::default()));
    let core = deepgram::run_realtime_core(
        sidecar.endpoints(),
        sidecar.token.clone(),
        flux_options,
        language,
        inner_rx,
        deepgram::realtime_emitter(app.clone(), shown.clone(), deepgram::ShownUntil::default()),
    );
    let mut recent = ReplayBuffer::default();
    let (rest, result) = relay(receiver, inner_tx, core, |chunk: &AudioChunk| recent.push(chunk)).await;

    crate::live_assist::detach_sidecar(generation);
    control.close();
    let exit = sidecar.shutdown().await;
    match (rest, result) {
        (_, Ok(())) => Outcome::Finished(Ok(())),
        (Some(rest), Err(e)) => {
            let reason = match exit {
                Some(code) => format!("{e}; sidecar {code}"),
                None => e,
            };
            let shown = *shown.lock().unwrap();
            info!("Pipecat live engine: replaying {:.1} s of recent audio to Direct", recent.seconds());
            Outcome::FallBack { receiver: recent.then(rest), reason, shown }
        }
        (None, Err(e)) => Outcome::Finished(Err(format!("Pipecat live engine: {e}"))),
    }
}

/// The last REPLAY_SECS of audio sent to the Pipecat engine.
#[derive(Default)]
struct ReplayBuffer {
    chunks: std::collections::VecDeque<AudioChunk>,
}

impl ReplayBuffer {
    fn push(&mut self, chunk: &AudioChunk) {
        self.chunks.push_back(chunk.clone());
        while self.chunks.front().is_some_and(|c| chunk.timestamp - c.timestamp > REPLAY_SECS) {
            self.chunks.pop_front();
        }
    }

    fn seconds(&self) -> f64 {
        match (self.chunks.front(), self.chunks.back()) {
            (Some(first), Some(last)) => last.timestamp - first.timestamp,
            _ => 0.0,
        }
    }

    /// A receiver that yields the buffered chunks, then everything `rest` yields.
    fn then(self, mut rest: mpsc::UnboundedReceiver<AudioChunk>) -> mpsc::UnboundedReceiver<AudioChunk> {
        let (tx, rx) = mpsc::unbounded_channel();
        for chunk in self.chunks {
            let _ = tx.send(chunk);
        }
        tokio::spawn(async move {
            while let Some(chunk) = rest.recv().await {
                if tx.send(chunk).is_err() {
                    return;
                }
            }
        });
        rx
    }
}

/// Forward the recording's chunks to `core` until the recording ends (then
/// close `inner` so the core drains and finishes) or the core ends first.
///
/// Returns the recording's receiver when the core ended while the recording
/// was still going, so the caller can continue on Direct, with the core's
/// result. A core that ends "cleanly" before the recording did is reported as
/// an error: the sidecar went away, and the rest of the call would otherwise
/// go untranscribed.
pub(crate) async fn relay<T, F, S>(
    mut outer: mpsc::UnboundedReceiver<T>,
    inner: mpsc::UnboundedSender<T>,
    core: F,
    mut sent: S,
) -> (Option<mpsc::UnboundedReceiver<T>>, Result<(), String>)
where
    F: std::future::Future<Output = Result<(), String>>,
    S: FnMut(&T),
{
    tokio::pin!(core);
    let mut inner = Some(inner);
    loop {
        tokio::select! {
            result = &mut core => {
                return match (inner.is_some(), result) {
                    (false, result) => (None, result),
                    (true, Ok(())) => (Some(outer), Err("the sidecar closed its connection mid-recording".to_string())),
                    (true, Err(e)) => (Some(outer), Err(e)),
                };
            }
            chunk = outer.recv(), if inner.is_some() => match chunk {
                Some(chunk) => {
                    sent(&chunk);
                    // A send error means the core dropped its receiver; the
                    // core's own result arrives on the next turn of the loop.
                    let _ = inner.as_ref().map(|tx| tx.send(chunk));
                }
                None => inner = None,
            },
        }
    }
}

// ============================================================================
// SIDECAR PROCESS
// ============================================================================

/// How to start the sidecar, or build its environment.
///
/// Running execs the environment's own `python` (built ahead by `uv sync`),
/// NOT `uv run`: under `uv run` the process Rust holds is uv, and killing it
/// (`kill_on_drop`, a crash test's SIGKILL) left the Python sidecar running
/// as an orphan with its sockets open (measured 2026-11-18). The PID Rust
/// holds must be the sidecar itself.
#[derive(Debug, Clone)]
pub(crate) struct SidecarCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub dir: PathBuf,
    pub env: Vec<(String, String)>,
}

impl SidecarCommand {
    /// `sync`: build the environment (`uv sync`) instead of running.
    fn resolve<R: Runtime>(app: &AppHandle<R>, sync: bool) -> Result<Self, String> {
        let dir = project_dir(app)?;
        let venv = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("no app data dir: {e}"))?
            .join("pipecat-sidecar")
            .join(".venv");
        if sync {
            let uv = find_uv()
                .ok_or("uv was not found (it ships with Meetily; this build lacks it), so the Pipecat engine cannot be set up")?;
            Ok(Self::sync(uv, dir, venv))
        } else {
            Self::run(dir, &venv)
        }
    }

    /// `uv sync` of the bundled project into `venv`, from the lock, no dev deps.
    pub(crate) fn sync(uv: PathBuf, dir: PathBuf, venv: PathBuf) -> Self {
        let project = dir.to_string_lossy().to_string();
        Self {
            program: uv,
            args: ["sync", "--frozen", "--no-dev", "--project", &project].map(String::from).to_vec(),
            dir,
            env: vec![
                // The bundled project is read-only inside the AppImage: keep the
                // environment out of it.
                ("UV_PROJECT_ENVIRONMENT".into(), venv.to_string_lossy().to_string()),
                ("UV_NO_PROGRESS".into(), "1".into()),
            ],
        }
    }

    /// The sidecar itself on `venv`'s python. Fails at once when the
    /// environment has not been built: a recording never waits on a download.
    pub(crate) fn run(dir: PathBuf, venv: &Path) -> Result<Self, String> {
        let python = venv.join("bin").join("python");
        if !python.exists() {
            return Err("the Pipecat engine is still being set up (its Python environment is not built yet)".to_string());
        }
        Ok(Self {
            program: python,
            args: ["-m", "meetily_sidecar"].map(String::from).to_vec(),
            env: vec![
                ("PYTHONPATH".into(), dir.to_string_lossy().to_string()),
                // No __pycache__ in the read-only bundle.
                ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
            ],
            dir,
        })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .current_dir(&self.dir)
            // DO NOT inherit the app's environment: inside the AppImage it
            // carries PYTHONHOME/PYTHONPATH pointing into the AppImage mount
            // (and an LD_LIBRARY_PATH for its bundled libs), and the sidecar's
            // python then dies with "No module named 'encodings'" (measured
            // 2026-11-18 on the first AppImage live run).
            .env_clear()
            .envs(inherited_env(std::env::vars()))
            .envs(self.env.iter().cloned())
            .kill_on_drop(true);
        cmd
    }
}

/// The parts of the app's environment the sidecar and uv may see: identity,
/// locale, PATH (uv finds its managed pythons), TLS trust and proxies.
pub(crate) fn inherited_env(vars: impl Iterator<Item = (String, String)>) -> Vec<(String, String)> {
    const KEEP: &[&str] = &[
        "HOME", "USER", "LOGNAME", "PATH", "TMPDIR", "LANG", "LANGUAGE", "TZ",
        "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE",
        "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy", "ALL_PROXY", "all_proxy",
        "XDG_CACHE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR",
        "UV_CACHE_DIR", "UV_PYTHON_INSTALL_DIR", "MEETILY_SIDECAR_LOG",
    ];
    vars.filter(|(k, _)| KEEP.contains(&k.as_str()) || k.starts_with("LC_")).collect()
}

/// The bundled sidecar project: `MEETILY_PIPECAT_SIDECAR_DIR`, else the app's
/// resources, else (debug builds) the source tree.
fn project_dir<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Ok(dir) = std::env::var("MEETILY_PIPECAT_SIDECAR_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join("pipecat-sidecar"));
    }
    if cfg!(debug_assertions) {
        candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("pipecat-sidecar"));
    }
    candidates
        .into_iter()
        .find(|d| d.join("pyproject.toml").is_file() && d.join("uv.lock").is_file())
        .ok_or_else(|| "the Pipecat sidecar is not bundled with this build".to_string())
}

/// uv for `uv sync`: `MEETILY_UV`, else the copy bundled next to Meetily's
/// executable (Tauri `externalBin`, build/uv.rs; WORK-559: Meetily stays one
/// app to launch, with no uv to install), else one on PATH or in the usual
/// install folders (development builds).
fn find_uv() -> Option<PathBuf> {
    if let Ok(uv) = std::env::var("MEETILY_UV") {
        return Some(PathBuf::from(uv)).filter(|p| p.is_file());
    }
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    find_uv_in(exe_dir.as_deref())
}

fn find_uv_in(exe_dir: Option<&Path>) -> Option<PathBuf> {
    let name = if cfg!(windows) { "uv.exe" } else { "uv" };
    if let Some(bundled) = exe_dir.map(|d| d.join(name)).filter(|p| p.is_file()) {
        return Some(bundled);
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    dirs.extend(["/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// A running sidecar. Dropping it kills the process (`kill_on_drop`);
/// `shutdown` is the orderly path.
pub(crate) struct Sidecar {
    child: Child,
    stdin: Option<ChildStdin>,
    pub port: u16,
    pub token: String,
}

#[derive(Deserialize)]
struct Ready {
    port: u16,
}

impl Sidecar {
    /// Start the sidecar and wait for its port. Secrets go over stdin only.
    pub(crate) async fn spawn(command: SidecarCommand, deepgram_api_key: &str, timeout: Duration) -> Result<Self, String> {
        let mut child = command
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start the sidecar ({}): {e}", command.program.display()))?;
        let mut stdin = child.stdin.take().ok_or("sidecar stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("sidecar stdout unavailable")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_logs(stderr));
        }
        let token = new_token();
        let boot = serde_json::json!({ "token": token, "deepgram_api_key": deepgram_api_key }).to_string() + "\n";
        stdin
            .write_all(boot.as_bytes())
            .await
            .map_err(|e| format!("could not hand the sidecar its config: {e}"))?;
        stdin.flush().await.map_err(|e| format!("could not hand the sidecar its config: {e}"))?;

        let mut lines = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(timeout, lines.next_line()).await;
        let line = match ready {
            Err(_) => Err(format!("the sidecar did not start within {} s", timeout.as_secs())),
            Ok(Err(e)) => Err(format!("could not read from the sidecar: {e}")),
            Ok(Ok(None)) => {
                let status = child.wait().await.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                Err(format!("the sidecar exited during startup ({status}); see the pipecat-sidecar log lines"))
            }
            Ok(Ok(Some(line))) => Ok(line),
        };
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                let _ = child.start_kill();
                return Err(e);
            }
        };
        let port = match serde_json::from_str::<Ready>(&line) {
            Ok(ready) => ready.port,
            Err(_) => {
                let _ = child.start_kill();
                return Err(format!("unexpected sidecar handshake: {:?}", line.chars().take(80).collect::<String>()));
            }
        };
        Ok(Self { child, stdin: Some(stdin), port, token })
    }

    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub(crate) fn endpoints(&self) -> RealtimeEndpoints {
        RealtimeEndpoints {
            // The sidecar speaks Flux only; nova never connects here.
            nova: format!("ws://127.0.0.1:{}/v1/unsupported", self.port),
            flux: format!("ws://127.0.0.1:{}/v2/listen", self.port),
            tag_lane_side: true,
        }
    }

    pub(crate) fn request(&self, path: &str) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
        let mut request = format!("ws://127.0.0.1:{}{}", self.port, path)
            .into_client_request()
            .map_err(|e| e.to_string())?;
        let auth = HeaderValue::from_str(&format!("Token {}", self.token)).map_err(|e| e.to_string())?;
        request.headers_mut().insert("Authorization", auth);
        Ok(request)
    }

    /// Open `/control`: text messages to the sidecar go through the returned
    /// sender; messages from it go to `live_assist`.
    async fn connect_control<R: Runtime>(&self, app: AppHandle<R>, generation: u64) -> Result<Control, String> {
        let request = self.request("/control")?;
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| format!("could not open the sidecar control connection: {e}"))?;
        let (mut write, mut read) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let closing = Arc::new(AtomicBool::new(false));
        let writer_closing = closing.clone();
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if let Err(e) = write.send(Message::Text(text)).await {
                    if !writer_closing.load(Ordering::SeqCst) {
                        warn!("Pipecat live engine: control send failed: {e}");
                    }
                    return;
                }
            }
            let _ = write.close().await;
        });
        let reader_closing = closing.clone();
        let ended = Arc::new(AtomicBool::new(false));
        let reader_ended = ended.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = read.next().await {
                if let Message::Text(text) = message {
                    crate::live_assist::on_sidecar_message(generation, &text);
                }
            }
            reader_ended.store(true, Ordering::SeqCst);
            if !reader_closing.load(Ordering::SeqCst) {
                // The sidecar still transcribes, but nothing can reach assist
                // any more (and it turns its own calls off): hand assist back
                // to the Rust timer rather than leave it silently dead.
                warn!("Pipecat live engine: control connection lost; live assist uses the Rust timer");
                crate::live_assist::detach_sidecar(generation);
                let _ = app.emit(
                    crate::live_assist::ASSIST_STATUS_EVENT,
                    serde_json::json!({ "state": "error", "message": "lost the connection to the Pipecat engine; live assist continues on the timer" }),
                );
            }
        });
        Ok(Control { tx, closing, ended })
    }

    /// Close stdin (the sidecar exits on EOF), wait, kill if it lingers.
    /// Returns how it ended, for the log and the fallback reason.
    pub(crate) async fn shutdown(mut self) -> Option<String> {
        drop(self.stdin.take());
        let status = match tokio::time::timeout(SHUTDOWN_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => status.to_string(),
            Ok(Err(e)) => format!("wait failed: {e}"),
            Err(_) => {
                warn!("Pipecat live engine: sidecar did not exit within {} s; killing it", SHUTDOWN_TIMEOUT.as_secs());
                let _ = self.child.kill().await;
                "killed after shutdown timeout".to_string()
            }
        };
        info!("Pipecat live engine: sidecar {status}");
        Some(status)
    }
}

struct Control {
    tx: mpsc::UnboundedSender<String>,
    closing: Arc<AtomicBool>,
    /// The reader saw the connection end.
    ended: Arc<AtomicBool>,
}

impl Control {
    fn close(self) {
        self.closing.store(true, Ordering::SeqCst);
        drop(self.tx);
    }
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn forward_logs(stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match sidecar_log_level(&line) {
            log::Level::Error => error!("pipecat-sidecar: {line}"),
            log::Level::Warn => warn!("pipecat-sidecar: {line}"),
            _ => info!("pipecat-sidecar: {line}"),
        }
    }
}

/// Level for one sidecar stderr line. Loguru lines carry `| LEVEL |`; any
/// other line (an interpreter crash: "Fatal Python error", a traceback) is
/// a warning, never INFO, so a sidecar that dies at startup stands out.
fn sidecar_log_level(line: &str) -> log::Level {
    let loguru = |level: &str| line.contains(&format!("| {level}"));
    if loguru("ERROR") || loguru("CRITICAL") {
        log::Level::Error
    } else if loguru("WARNING") {
        log::Level::Warn
    } else if loguru("INFO") || loguru("DEBUG") || loguru("TRACE") || loguru("SUCCESS") {
        log::Level::Info
    } else {
        log::Level::Warn
    }
}

// ============================================================================
// PREPARE (build the Python environment ahead of the first recording)
// ============================================================================

static PREPARING: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone)]
struct PrepareStatus {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Build (or verify) the sidecar environment with `uv sync --frozen`. Safe to
/// call repeatedly; a second call while one runs returns at once.
pub async fn prepare<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    if PREPARING.swap(true, Ordering::SeqCst) {
        return Err("already preparing".to_string());
    }
    let _ = app.emit(PREPARE_EVENT, PrepareStatus { state: "preparing", message: None });
    let result = async {
        let command = SidecarCommand::resolve(app, true)?;
        // Which uv: the bundled one (next to the executable) or a fallback.
        info!("Pipecat live engine: preparing the environment with {}", command.program.display());
        let started = std::time::Instant::now();
        let output = command
            .command()
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("could not run uv: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("uv sync failed ({}): {}", output.status, stderr.trim()));
        }
        info!("Pipecat live engine: environment ready in {} ms", started.elapsed().as_millis());
        Ok(())
    }
    .await;
    PREPARING.store(false, Ordering::SeqCst);
    let status = match &result {
        Ok(()) => PrepareStatus { state: "ready", message: None },
        Err(e) => {
            error!("Pipecat live engine: {e}");
            PrepareStatus { state: "error", message: Some(e.clone()) }
        }
    };
    let _ = app.emit(PREPARE_EVENT, status);
    result
}

fn spawn_prepare<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let _ = prepare(&app).await;
    });
}

/// At app start: if the Pipecat engine is selected, make sure its
/// environment is built before the first recording needs it.
pub fn prepare_if_selected<R: Runtime>(app: &AppHandle<R>) {
    if load_engine(app) == LiveEngine::Pipecat {
        spawn_prepare(app.clone());
    }
}

// ============================================================================
// COMMANDS
// ============================================================================

#[tauri::command]
pub async fn live_engine_get<R: Runtime>(app: AppHandle<R>) -> LiveEngine {
    load_engine(&app)
}

/// Save the engine for the next recording (a running one keeps its engine).
/// Switching to Pipecat builds its environment in the background.
#[tauri::command]
pub async fn live_engine_set<R: Runtime>(app: AppHandle<R>, engine: LiveEngine) -> Result<(), String> {
    let store = app.store(STORE).map_err(|e| format!("settings store unavailable: {e}"))?;
    store.set(ENGINE_KEY, serde_json::to_value(engine).map_err(|e| e.to_string())?);
    store.save().map_err(|e| format!("could not save the setting: {e}"))?;
    info!("live engine set to {engine:?}");
    if engine == LiveEngine::Pipecat {
        spawn_prepare(app);
    }
    Ok(())
}

/// Build the sidecar environment now and report the outcome (the on-demand
/// path of what switching the engine and app start do in the background).
#[tauri::command]
pub async fn live_engine_prepare<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    prepare(&app).await
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn bundled_uv_next_to_the_executable_wins() {
        let dir = std::env::temp_dir().join(format!("meetily-uv-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "uv.exe" } else { "uv" };
        std::fs::write(dir.join(name), b"").unwrap();
        assert_eq!(super::find_uv_in(Some(&dir)), Some(dir.join(name)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use super::*;

    #[tokio::test]
    async fn relay_forwards_until_the_recording_ends_then_returns_the_core_result() {
        let (outer_tx, outer_rx) = mpsc::unbounded_channel::<u32>();
        let (inner_tx, mut inner_rx) = mpsc::unbounded_channel::<u32>();
        let core = async move {
            let mut got = Vec::new();
            while let Some(x) = inner_rx.recv().await {
                got.push(x);
            }
            assert_eq!(got, vec![1, 2, 3]);
            Ok(())
        };
        for x in [1, 2, 3] {
            outer_tx.send(x).unwrap();
        }
        drop(outer_tx);
        let (rest, result) = relay(outer_rx, inner_tx, core, |_| {}).await;
        assert!(rest.is_none(), "recording ended: nothing to fall back with");
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn a_core_failing_mid_recording_hands_back_the_rest_of_the_audio() {
        let (outer_tx, outer_rx) = mpsc::unbounded_channel::<u32>();
        let (inner_tx, mut inner_rx) = mpsc::unbounded_channel::<u32>();
        let core = async move {
            inner_rx.recv().await;
            Err("Deepgram Flux realtime error: boom".to_string())
        };
        outer_tx.send(1).unwrap();
        let (rest, result) = relay(outer_rx, inner_tx, core, |_| {}).await;
        assert_eq!(result, Err("Deepgram Flux realtime error: boom".to_string()));
        let mut rest = rest.expect("the recording continues on Direct");
        outer_tx.send(2).unwrap();
        assert_eq!(rest.recv().await, Some(2), "later audio reaches the fallback");
    }

    #[tokio::test]
    async fn a_core_ending_cleanly_mid_recording_is_still_a_fallback() {
        let (_outer_tx, outer_rx) = mpsc::unbounded_channel::<u32>();
        let (inner_tx, _inner_rx) = mpsc::unbounded_channel::<u32>();
        let (rest, result) = relay(outer_rx, inner_tx, async { Ok(()) }, |_| {}).await;
        assert!(rest.is_some());
        assert!(result.unwrap_err().contains("mid-recording"));
    }

    #[tokio::test]
    async fn a_core_failing_while_draining_after_stop_is_a_plain_error() {
        let (outer_tx, outer_rx) = mpsc::unbounded_channel::<u32>();
        let (inner_tx, mut inner_rx) = mpsc::unbounded_channel::<u32>();
        drop(outer_tx);
        let core = async move {
            while inner_rx.recv().await.is_some() {}
            Err("final responses lost".to_string())
        };
        let (rest, result) = relay(outer_rx, inner_tx, core, |_| {}).await;
        assert!(rest.is_none());
        assert_eq!(result, Err("final responses lost".to_string()));
    }

    #[test]
    fn run_execs_the_environment_python_and_sync_keeps_state_out_of_the_bundle() {
        let venv = tempfile::tempdir().unwrap();
        let err = SidecarCommand::run("/opt/app/pipecat-sidecar".into(), venv.path()).unwrap_err();
        assert!(err.contains("still being set up"), "{err}");
        std::fs::create_dir_all(venv.path().join("bin")).unwrap();
        std::fs::write(venv.path().join("bin/python"), b"").unwrap();
        let run = SidecarCommand::run("/opt/app/pipecat-sidecar".into(), venv.path()).unwrap();
        assert_eq!(run.program, venv.path().join("bin/python"), "the held PID is the sidecar, not uv");
        assert_eq!(run.args, vec!["-m", "meetily_sidecar"]);
        assert!(run.env.contains(&("PYTHONDONTWRITEBYTECODE".into(), "1".into())));
        let sync = SidecarCommand::sync("/usr/bin/uv".into(), "/opt/app/pipecat-sidecar".into(), "/data/venv".into());
        assert_eq!(sync.args[..3], ["sync", "--frozen", "--no-dev"]);
        assert!(sync.env.contains(&("UV_PROJECT_ENVIRONMENT".into(), "/data/venv".into())));
    }

    #[test]
    fn the_appimage_python_environment_never_reaches_the_sidecar() {
        let parent = [
            ("PYTHONHOME", "/tmp/.mount_X/usr/"),
            ("PYTHONPATH", "/tmp/.mount_X/usr/lib/python3.12"),
            ("LD_LIBRARY_PATH", "/tmp/.mount_X/usr/lib"),
            ("APPDIR", "/tmp/.mount_X"),
            ("HOME", "/home/u"),
            ("PATH", "/usr/bin"),
            ("LC_ALL", "C.UTF-8"),
            ("SSL_CERT_FILE", "/etc/ssl/cert.pem"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let kept: Vec<String> = inherited_env(parent.into_iter()).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kept, vec!["HOME", "PATH", "LC_ALL", "SSL_CERT_FILE"]);
    }

    #[tokio::test]
    async fn a_fallback_replays_the_recent_audio_before_the_rest() {
        let chunk = |t: f64| AudioChunk {
            data: vec![0.0; 160],
            sample_rate: 16_000,
            timestamp: t,
            chunk_id: (t * 10.0) as u64,
            device_type: crate::audio::recording_state::DeviceType::Microphone,
            layout: crate::audio::recording_state::AudioLayout::MicOnly,
        };
        let mut recent = ReplayBuffer::default();
        for t in [0.0, 10.0, 25.0, 40.0, 45.0] {
            recent.push(&chunk(t));
        }
        assert_eq!(recent.seconds(), 20.0, "kept 25..45: older than REPLAY_SECS behind the newest is dropped");
        let (tx, rest) = mpsc::unbounded_channel();
        tx.send(chunk(46.0)).unwrap();
        drop(tx);
        let mut rx = recent.then(rest);
        let mut got = Vec::new();
        while let Some(c) = rx.recv().await {
            got.push(c.timestamp);
        }
        assert_eq!(got, vec![25.0, 40.0, 45.0, 46.0], "the replay, then the rest of the recording, in order");
    }

    #[test]
    fn sidecar_crash_output_is_never_logged_as_info() {
        assert_eq!(sidecar_log_level("2026-11-18 10:38:03.870 | INFO     | __main__:on_ready:60 - listening"), log::Level::Info);
        assert_eq!(sidecar_log_level("2026-11-18 10:38:03.870 | WARNING  | x - y"), log::Level::Warn);
        assert_eq!(sidecar_log_level("2026-11-18 10:38:03.870 | ERROR    | x - y"), log::Level::Error);
        assert_eq!(sidecar_log_level("Fatal Python error: init_fs_encoding: failed to get the Python codec"), log::Level::Warn);
        assert_eq!(sidecar_log_level("ModuleNotFoundError: No module named 'encodings'"), log::Level::Warn);
    }

    #[test]
    fn tokens_are_random_and_long() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn frontend_matches_the_live_engine_contract() {
        let lib = include_str!("../../../src/lib/live-engine.ts");
        assert!(lib.contains(&format!("'{PREPARE_EVENT}'")), "frontend does not listen for {PREPARE_EVENT}");
        assert!(lib.contains("'direct' | 'pipecat'"));
        let ui = include_str!("../../../src/components/LiveEngineSettings.tsx");
        for command in ["live_engine_get", "live_engine_set"] {
            assert!(ui.contains(&format!("'{command}'")), "settings UI does not call {command}");
        }
    }

    #[test]
    fn the_bundle_carries_the_sidecar_project() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let resources: Vec<&str> = conf["bundle"]["resources"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        for needed in ["pipecat-sidecar/pyproject.toml", "pipecat-sidecar/uv.lock", "pipecat-sidecar/meetily_sidecar/*.py"] {
            assert!(resources.contains(&needed), "tauri.conf.json bundle.resources lacks {needed}");
        }
    }

    #[test]
    fn engine_serializes_as_the_frontend_expects() {
        assert_eq!(serde_json::to_value(LiveEngine::Pipecat).unwrap(), serde_json::json!("pipecat"));
        assert_eq!(serde_json::to_value(LiveEngine::Direct).unwrap(), serde_json::json!("direct"));
    }

    // ---- process tests (real sidecar with its fake Flux; need uv and the
    // synced dev environment: `uv sync` in pipecat-sidecar) ----------------

    pub(crate) fn dev_command() -> SidecarCommand {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("pipecat-sidecar");
        let mut cmd = SidecarCommand::run(dir.clone(), &dir.join(".venv")).expect("run `uv sync` in pipecat-sidecar");
        cmd.env.push(("MEETILY_SIDECAR_FAKE_FLUX".into(), "1".into()));
        cmd
    }

    pub(crate) const TEST_STARTUP_TIMEOUT: Duration = STARTUP_TIMEOUT;

    #[tokio::test]
    #[ignore = "starts the real sidecar process: needs uv and `uv sync` in pipecat-sidecar"]
    async fn sidecar_starts_authenticates_and_stops() {
        let sidecar = Sidecar::spawn(dev_command(), "dg-test", STARTUP_TIMEOUT).await.expect("starts");
        let pid = sidecar.pid().unwrap();
        // Wrong token is refused.
        let mut bad = sidecar.request("/control").unwrap();
        bad.headers_mut().insert("Authorization", HeaderValue::from_static("Token nope"));
        assert!(tokio_tungstenite::connect_async(bad).await.is_err());
        // Right token is accepted.
        let (ws, _) = tokio_tungstenite::connect_async(sidecar.request("/control").unwrap()).await.expect("control");
        drop(ws);
        let status = sidecar.shutdown().await.unwrap();
        assert!(status.contains("exit status: 0"), "{status}");
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "the process is gone");
    }

    #[tokio::test]
    #[ignore = "starts the real sidecar process: needs uv and `uv sync` in pipecat-sidecar"]
    async fn dropping_the_handle_leaves_no_sidecar_behind() {
        let sidecar = Sidecar::spawn(dev_command(), "dg-test", STARTUP_TIMEOUT).await.expect("starts");
        let pid = sidecar.pid().unwrap();
        drop(sidecar);
        for _ in 0..50 {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            // Gone, or a zombie waiting for the reaper: either way not running.
            if state.is_empty() || state.contains(") Z ") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("sidecar {pid} still running after its handle was dropped");
    }

    #[tokio::test]
    #[ignore = "starts the real sidecar process: needs uv and `uv sync` in pipecat-sidecar"]
    async fn a_sidecar_that_fails_its_handshake_is_reported() {
        let mut cmd = dev_command();
        cmd.args = vec!["-c".into(), "print('hello')".into()];
        let err = Sidecar::spawn(cmd, "k", STARTUP_TIMEOUT).await.err().expect("not a sidecar");
        assert!(err.contains("unexpected sidecar handshake"), "{err}");
        let mut cmd = dev_command();
        cmd.program = "/nonexistent/python".into();
        assert!(Sidecar::spawn(cmd, "k", STARTUP_TIMEOUT).await.err().unwrap().contains("could not start"));
    }
}
