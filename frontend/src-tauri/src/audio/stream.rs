use std::sync::Arc;
use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Stream, SupportedStreamConfig};
use log::{error, info, warn};
use tokio::sync::mpsc;

use super::devices::{AudioDevice, get_device_and_config};
use super::pipeline::AudioCapture;
use super::recording_state::{RecordingState, DeviceType};
use super::capture::{AudioCaptureBackend, get_current_backend};
use super::stream_slots::{StreamSlots, Supervised};

#[cfg(target_os = "macos")]
use super::capture::CoreAudioCapture;

/// Stream backend implementation
pub enum StreamBackend {
    /// CPAL-based stream (ScreenCaptureKit or default)
    Cpal(Stream),
    /// Core Audio direct implementation (macOS only)
    #[cfg(target_os = "macos")]
    CoreAudio {
        task: Option<tokio::task::JoinHandle<()>>,
    },
}

// SAFETY: While Stream doesn't implement Send, we ensure it's only accessed
// from the same thread context by using spawn_blocking for operations that cross thread boundaries
unsafe impl Send for StreamBackend {}

/// Simplified audio stream wrapper with multi-backend support
pub struct AudioStream {
    device: Arc<AudioDevice>,
    backend: StreamBackend,
    /// `AudioCapture::stream_id` of the capture feeding this stream.
    capture_id: u64,
    /// The capture feeding it; asked whether the stream already died.
    capture: AudioCapture,
}

// SAFETY: AudioStream contains StreamBackend which we've marked as Send
unsafe impl Send for AudioStream {}

impl AudioStream {
    /// Create a new audio stream for the given device
    pub async fn create(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<Self> {
        // Get current backend from global config
        let backend_type = get_current_backend();
        Self::create_with_backend(device, state, device_type, recording_sender, backend_type).await
    }

    /// `create` on a blocking thread. Opening a device runs synchronous cpal
    /// calls (enumeration, stream build, play) that can stall on a half-dead
    /// backend; on an async worker that would block the runtime and make any
    /// `tokio::time::timeout` around it unable to fire. The reopen path and
    /// the live harness both use this one entry point. A stalled attempt's
    /// stream is dropped when it finally returns.
    pub async fn create_blocking(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            runtime.block_on(Self::create(device, state, device_type, None))
        })
        .await
        .map_err(|e| anyhow::anyhow!("stream open task failed: {}", e))?
    }

    /// Create a new audio stream with explicit backend selection
    pub async fn create_with_backend(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
        backend_type: AudioCaptureBackend,
    ) -> Result<Self> {
        info!("🎵 Stream: Creating audio stream for device: {} with backend: {:?}, device_type: {:?}",
              device.name, backend_type, device_type);

        // For system audio devices, use the selected backend
        // For microphone devices, always use CPAL
        #[cfg(target_os = "macos")]
        let use_core_audio = device_type == DeviceType::System
            && backend_type == AudioCaptureBackend::CoreAudio;

        #[cfg(not(target_os = "macos"))]
        let use_core_audio = false;

        #[cfg(target_os = "macos")]
        info!("🎵 Stream: use_core_audio = {}, device_type == System: {}, backend == CoreAudio: {}",
              use_core_audio,
              device_type == DeviceType::System,
              backend_type == AudioCaptureBackend::CoreAudio);

        #[cfg(not(target_os = "macos"))]
        info!("🎵 Stream: use_core_audio = {}, device_type == System: {}",
              use_core_audio,
              device_type == DeviceType::System);

        #[cfg(target_os = "macos")]
        if use_core_audio {
            info!("🎵 Stream: Using Core Audio backend (cidre) for system audio");
            return Self::create_core_audio_stream(device, state, device_type, recording_sender).await;
        }

        // Default path: use CPAL
        #[cfg(target_os = "macos")]
        let backend_name = if backend_type == AudioCaptureBackend::ScreenCaptureKit {
            "ScreenCaptureKit"
        } else {
            "CPAL (default)"
        };

        #[cfg(not(target_os = "macos"))]
        let backend_name = "CPAL";

        info!("🎵 Stream: Using CPAL backend ({}) for device: {}", backend_name, device.name);
        Self::create_cpal_stream(device, state, device_type, recording_sender).await
    }

    /// Create a CPAL-based stream (ScreenCaptureKit on macOS)
    async fn create_cpal_stream(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<Self> {
        info!("Creating CPAL stream for device: {}", device.name);

        // Get the underlying cpal device and config
        let (cpal_device, config) = get_device_and_config(&device).await?;

        info!("Audio config - Sample rate: {}, Channels: {}, Format: {:?}",
              config.sample_rate().0, config.channels(), config.sample_format());

        // Create audio capture processor
        let capture = AudioCapture::new(
            device.clone(),
            state.clone(),
            config.sample_rate().0,
            config.channels(),
            device_type,
            recording_sender,
        );

        // Build the appropriate stream based on sample format
        let stream = Self::build_stream(&cpal_device, &config, capture.clone())?;

        // Start the stream
        stream.play()?;
        info!("CPAL stream started for device: {}", device.name);

        Ok(Self {
            device,
            backend: StreamBackend::Cpal(stream),
            capture_id: capture.stream_id(),
            capture: capture.clone(),
        })
    }

    /// Create a Core Audio stream (macOS only)
    #[cfg(target_os = "macos")]
    async fn create_core_audio_stream(
        device: Arc<AudioDevice>,
        state: Arc<RecordingState>,
        device_type: DeviceType,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<Self> {
        info!("🔊 Stream: Creating Core Audio stream for device: {}", device.name);

        // Create Core Audio capture
        info!("🔊 Stream: Calling CoreAudioCapture::new()...");
        let capture_impl = CoreAudioCapture::new()
            .map_err(|e| {
                error!("❌ Stream: CoreAudioCapture::new() failed: {}", e);
                anyhow::anyhow!("Failed to create Core Audio capture: {}", e)
            })?;

        info!("✅ Stream: CoreAudioCapture created, calling stream()...");
        let core_stream = capture_impl.stream()
            .map_err(|e| {
                error!("❌ Stream: capture_impl.stream() failed: {}", e);
                anyhow::anyhow!("Failed to create Core Audio stream: {}", e)
            })?;

        let sample_rate = core_stream.sample_rate();
        info!("✅ Stream: Core Audio stream created with sample rate: {} Hz", sample_rate);

        // Create audio capture processor for pipeline integration
        // CRITICAL: Core Audio tap is MONO (with_mono_global_tap_excluding_processes)
        let capture = AudioCapture::new(
            device.clone(),
            state.clone(),
            sample_rate,
            1, // Core Audio tap is MONO (not stereo!)
            device_type,
            recording_sender,
        );

        // Spawn task to process Core Audio stream samples
        // The stream needs to be polled continuously to produce samples
        let device_name = device.name.clone();
        info!("🔊 Stream: Spawning tokio task to poll Core Audio stream...");
        let task = tokio::spawn({
            let capture = capture.clone();
            let mut stream = core_stream;

            async move {
                use futures_util::StreamExt;

                let mut buffer = Vec::new();
                let mut frame_count = 0;
                let frames_per_chunk = 1024; // Process in chunks of 1024 samples

                info!("✅ Stream: Core Audio processing task started for {}", device_name);

                let mut _sample_count = 0u64;
                while let Some(sample) = stream.next().await {
                    _sample_count += 1;
                    // if _sample_count % 48000 == 0 {
                    //     info!("📊 Stream: Received {} samples from Core Audio stream", _sample_count);
                    // }

                    buffer.push(sample);
                    frame_count += 1;

                    // Process when we have enough samples
                    if frame_count >= frames_per_chunk {
                        capture.process_audio_data(&buffer);
                        buffer.clear();
                        frame_count = 0;
                    }
                }

                // Process any remaining samples
                if !buffer.is_empty() {
                    capture.process_audio_data(&buffer);
                }

                info!("⚠️ Stream: Core Audio processing task ended for {}", device_name);
            }
        });

        info!("✅ Stream: Core Audio stream fully initialized for device: {}", device.name);

        Ok(Self {
            device: device.clone(),
            backend: StreamBackend::CoreAudio {
                task: Some(task),
            },
            capture_id: capture.stream_id(),
            capture: capture.clone(),
        })
    }

    /// Build stream based on sample format
    fn build_stream(
        device: &Device,
        config: &SupportedStreamConfig,
        capture: AudioCapture,
    ) -> Result<Stream> {
        let config_copy = config.clone();

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        capture.process_audio_data(data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let f32_data: Vec<f32> = data.iter()
                            .map(|&sample| sample as f32 / i16::MAX as f32)
                            .collect();
                        capture.process_audio_data(&f32_data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I32 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[i32], _: &cpal::InputCallbackInfo| {
                        let f32_data: Vec<f32> = data.iter()
                            .map(|&sample| sample as f32 / i32::MAX as f32)
                            .collect();
                        capture.process_audio_data(&f32_data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            cpal::SampleFormat::I8 => {
                let capture_clone = capture.clone();
                device.build_input_stream(
                    &config_copy.into(),
                    move |data: &[i8], _: &cpal::InputCallbackInfo| {
                        let f32_data: Vec<f32> = data.iter()
                            .map(|&sample| sample as f32 / i8::MAX as f32)
                            .collect();
                        capture.process_audio_data(&f32_data);
                    },
                    move |err| {
                        capture_clone.handle_stream_error(err);
                    },
                    None,
                )?
            }
            _ => {
                return Err(anyhow::anyhow!("Unsupported sample format: {:?}", config.sample_format()));
            }
        };

        Ok(stream)
    }

    /// Identity matched against `StreamFailure::stream_id`.
    pub fn capture_id(&self) -> u64 {
        self.capture_id
    }

    /// Get device info
    pub fn device(&self) -> &AudioDevice {
        &self.device
    }

    /// Stop the stream
    pub fn stop(self) -> Result<()> {
        info!("Stopping audio stream for device: {}", self.device.name);

        match self.backend {
            StreamBackend::Cpal(stream) => {
                // CRITICAL: Pause the stream first to stop callbacks immediately
                // This ensures closures stop executing before we drop the stream,
                // allowing Arc references captured in callbacks to be released
                if let Err(e) = stream.pause() {
                    warn!("Failed to pause stream before drop: {}", e);
                }
                info!("Stream paused, now dropping to release callbacks");
                drop(stream);
            }
            #[cfg(target_os = "macos")]
            StreamBackend::CoreAudio { task } => {
                // Abort the processing task and wait briefly for cleanup
                if let Some(task_handle) = task {
                    info!("Aborting Core Audio task...");
                    task_handle.abort();
                    // Give the runtime a moment to clean up the aborted task
                    // This helps ensure Arc references in the closure are dropped
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    info!("Core Audio task aborted");
                }
            }
        }

        // Explicitly drop self.device Arc reference
        drop(self.device);
        info!("Audio stream stopped and device reference dropped");
        Ok(())
    }
}

impl Supervised for AudioStream {
    fn stream_id(&self) -> u64 {
        self.capture_id
    }

    fn is_dead(&self) -> bool {
        self.capture.is_dead()
    }
}

/// Audio stream manager for handling multiple streams. The stream slots live
/// in `StreamSlots`, the single owner of every slot mutation (start, stop, mic
/// swap, dead-stream recovery); see `stream_slots` for the epoch rules.
pub struct AudioStreamManager {
    slots: StreamSlots<AudioStream>,
    state: Arc<RecordingState>,
}

// SAFETY: AudioStreamManager contains AudioStream which we've marked as Send
unsafe impl Send for AudioStreamManager {}

impl AudioStreamManager {
    pub fn new(state: Arc<RecordingState>) -> Self {
        Self {
            slots: StreamSlots::new(),
            state,
        }
    }

    /// Place a stream via the start path and supervise it from its first
    /// moment: a stream that arrives already dead gets its failure reported.
    fn install_started(&mut self, kind: DeviceType, stream: AudioStream) {
        let (replaced, failure) = self.slots.install(kind, stream);
        drop(replaced);
        if let Some(failure) = failure {
            self.state.report_stream_failure(failure);
        }
    }

    /// Start audio streams for the given devices
    pub async fn start_streams(
        &mut self,
        microphone_device: Option<Arc<AudioDevice>>,
        system_device: Option<Arc<AudioDevice>>,
        recording_sender: Option<mpsc::UnboundedSender<super::recording_state::AudioChunk>>,
    ) -> Result<()> {
        use super::capture::get_current_backend;
        let backend = get_current_backend();
        info!("🎙️ Starting audio streams with backend: {:?}", backend);

        // Start microphone stream
        if let Some(mic_device) = microphone_device {
            info!("🎤 Creating microphone stream: {} (always uses CPAL)", mic_device.name);
            match AudioStream::create(mic_device.clone(), self.state.clone(), DeviceType::Microphone, recording_sender.clone()).await {
                Ok(stream) => {
                    self.state.set_microphone_device(mic_device);
                    self.install_started(DeviceType::Microphone, stream);
                    info!("✅ Microphone stream created successfully");
                }
                Err(e) => {
                    error!("❌ Failed to create microphone stream: {}", e);
                    return Err(e);
                }
            }
        } else {
            info!("ℹ️ No microphone device specified, skipping microphone stream");
        }

        // Start system audio stream
        if let Some(sys_device) = system_device {
            info!("🔊 Creating system audio stream: {} (backend: {:?})", sys_device.name, backend);
            match AudioStream::create(sys_device.clone(), self.state.clone(), DeviceType::System, recording_sender.clone()).await {
                Ok(stream) => {
                    self.state.set_system_device(sys_device);
                    self.install_started(DeviceType::System, stream);
                    info!("✅ System audio stream created with {:?} backend", backend);
                }
                Err(e) => {
                    warn!("⚠️ Failed to create system audio stream: {}", e);
                    // Don't fail if only system audio fails
                }
            }
        } else {
            info!("ℹ️ No system device specified, skipping system audio stream");
        }

        // Ensure at least one stream was created
        if self.slots.count() == 0 {
            return Err(anyhow::anyhow!("No audio streams could be created"));
        }

        Ok(())
    }

    /// Stop all audio streams
    pub fn stop_streams(&mut self) -> Result<()> {
        info!("Stopping all audio streams");

        let mut errors = Vec::new();

        if let Some(mic_stream) = self.slots.take(DeviceType::Microphone) {
            if let Err(e) = mic_stream.stop() {
                error!("Failed to stop microphone stream: {}", e);
                errors.push(e);
            }
        }

        if let Some(sys_stream) = self.slots.take(DeviceType::System) {
            if let Err(e) = sys_stream.stop() {
                error!("Failed to stop system stream: {}", e);
                errors.push(e);
            }
        }

        if !errors.is_empty() {
            Err(anyhow::anyhow!("Failed to stop some streams: {:?}", errors))
        } else {
            info!("All audio streams stopped successfully");
            Ok(())
        }
    }

    /// Mic hot-swap phase 1: take the microphone stream OUT of the manager,
    /// keeping system audio running. Bumps the slot epoch, so any dead-stream
    /// recovery already in flight for the mic becomes obsolete. The caller
    /// stops/drops it OUTSIDE any lock — a cpal teardown of a dead BT device
    /// can stall and must not block a held mutex.
    pub fn take_mic_stream(&mut self) -> Option<AudioStream> {
        self.slots.take(DeviceType::Microphone)
    }

    /// Mic hot-swap phase 3: install the new microphone stream. Returns the
    /// stream it replaced, if any (the caller tears it down OUTSIDE the lock).
    /// A stream that arrives already dead is reported like any other failure.
    pub fn set_mic_stream(&mut self, stream: AudioStream) -> Option<AudioStream> {
        let (replaced, failure) = self.slots.install(DeviceType::Microphone, stream);
        if let Some(failure) = failure {
            self.state.report_stream_failure(failure);
        }
        replaced
    }

    /// Recovery: take the stream out only if it is still the one that failed.
    /// Returns it with the slot epoch the recovery run must stay within.
    pub fn recovery_take(&mut self, kind: DeviceType, stream_id: u64) -> Option<(AudioStream, u64)> {
        self.slots.recovery_take(kind, stream_id)
    }

    /// Recovery: install the replacement only if nothing else touched the slot
    /// since `recovery_take`. Hands it back when the run is obsolete.
    pub fn recovery_install(&mut self, kind: DeviceType, epoch: u64, stream: AudioStream) -> Option<AudioStream> {
        match self.slots.recovery_install(kind, epoch, stream) {
            Ok(failure) => {
                if let Some(failure) = failure {
                    self.state.report_stream_failure(failure);
                }
                None
            }
            Err(stream) => Some(stream),
        }
    }

    /// Current epoch of `kind`'s slot (recovery stays valid only while unchanged).
    pub fn slot_epoch(&self, kind: DeviceType) -> u64 {
        self.slots.epoch(kind)
    }

    /// Get stream count
    pub fn active_stream_count(&self) -> usize {
        self.slots.count()
    }

    /// Check if any streams are active
    pub fn has_active_streams(&self) -> bool {
        self.slots.count() > 0
    }
}

impl Drop for AudioStreamManager {
    fn drop(&mut self) {
        if let Err(e) = self.stop_streams() {
            error!("Error stopping streams during drop: {}", e);
        }
    }
}

#[cfg(test)]
mod live_tests {
    //! Live reproduction of the W93 error flood. `#[ignore]`d: it needs a real
    //! PipeWire session and an ALSA PCM named by `MEETILY_W93_PCM`; run it via
    //! `scripts/w93-stream-kill.sh`, which builds a private null sink and
    //! destroys ONLY this test's PipeWire objects (never restarts the sound
    //! server). The recovery loop below wires the production pieces
    //! (`RecordingState` failure channel, `AudioStreamManager` slots,
    //! `recover_stream`) the same way `spawn_stream_recovery` does.
    use super::*;
    use crate::audio::stream_recovery::{recover_stream, RecoveryOutcome, RecoveryPolicy};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn live_stream_kill_flood() {
        let _ = env_logger::builder().filter_level(log::LevelFilter::Info).try_init();
        let pcm = std::env::var("MEETILY_W93_PCM").expect("MEETILY_W93_PCM");
        let observe_secs: u64 = std::env::var("MEETILY_W93_OBSERVE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        // MEETILY_W93_RECOVER=0 reproduces the pre-fix shape (no recovery task).
        let recover = std::env::var("MEETILY_W93_RECOVER").map_or(true, |v| v != "0");
        // `flood-stop` = what production ships (AUTO_REOPEN off); `recover` and
        // `exhaust` drive the full reopen machinery, which production keeps off.
        let expect = std::env::var("MEETILY_W93_EXPECT").unwrap_or_default();
        let policy = if expect == "flood-stop" { RecoveryPolicy::production() } else { RecoveryPolicy::full_reopen() };

        let state = RecordingState::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        state.set_audio_sender(tx);
        state.start_recording().unwrap();
        let callbacks = Arc::new(AtomicU64::new(0));
        let cb = callbacks.clone();
        state.set_error_callback(move |_| {
            cb.fetch_add(1, Ordering::SeqCst);
        });

        let chunks = Arc::new(AtomicU64::new(0));
        let c2 = chunks.clone();
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                c2.fetch_add(1, Ordering::SeqCst);
            }
        });

        let dev = Arc::new(AudioDevice::from_name(&format!("{pcm} (output)")).unwrap());
        let mgr = Arc::new(Mutex::new(AudioStreamManager::new(state.clone())));
        mgr.lock().unwrap().start_streams(None, Some(dev.clone()), None).await.expect("open stream");
        eprintln!("W93_READY pid={}", std::process::id());

        // A failure naming a stream that is not in the slot (already replaced)
        // must be ignored, not tear down the live one.
        state.report_stream_failure(crate::audio::recording_state::StreamFailure {
            kind: DeviceType::System,
            stream_id: u64::MAX,
        });

        let outcomes = Arc::new(Mutex::new(Vec::<String>::new()));
        let failure_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if recover {
            let mut failures = state.take_stream_failure_receiver().unwrap();
            let (state2, mgr2, outcomes2, dev2) = (state.clone(), mgr.clone(), outcomes.clone(), dev.clone());
            let seen2 = failure_seen.clone();
            tokio::spawn(async move {
                while let Some(crate::audio::recording_state::StreamFailure { kind, stream_id }) = failures.recv().await {
                    let dead = mgr2.lock().unwrap().recovery_take(kind, stream_id);
                    if dead.is_some() {
                        seen2.store(true, Ordering::SeqCst);
                    }
                    let Some((dead, epoch)) = dead else {
                        eprintln!("W93_STALE failure for stream {stream_id}");
                        continue;
                    };
                    let out = recover_stream(
                        kind,
                        policy,
                        move || Some(dead),
                        |s: AudioStream| async move {
                            let _ = tokio::task::spawn_blocking(move || {
                                let _ = s.stop();
                            })
                            .await;
                        },
                        || {
                            let (st, d) = (state2.clone(), dev2.clone());
                            async move { AudioStream::create_blocking(d, st, kind).await }
                        },
                        |s| mgr2.lock().unwrap().recovery_install(kind, epoch, s),
                        || state2.is_recording() && mgr2.lock().unwrap().slot_epoch(kind) == epoch,
                        tokio::time::sleep,
                    )
                    .await;
                    eprintln!("W93_OUTCOME {:?}", out);
                    outcomes2.lock().unwrap().push(format!("{:?}", out));
                    if let RecoveryOutcome::Exhausted { .. } = out {
                        state2.announce_error(crate::audio::recording_state::AudioError::DeviceDisconnected);
                    }
                }
            });
        }

        let mut chunk_history = Vec::new();
        for s in 0..observe_secs {
            tokio::time::sleep(Duration::from_secs(1)).await;
            chunk_history.push(chunks.load(Ordering::SeqCst));
            eprintln!(
                "W93_TICK t={}s chunks={} callbacks={} errors={} recording={} streams={}",
                s + 1,
                chunks.load(Ordering::SeqCst),
                callbacks.load(Ordering::SeqCst),
                state.get_error_count(),
                state.is_recording(),
                mgr.lock().unwrap().active_stream_count()
            );
        }
        let end_streams = mgr.lock().unwrap().active_stream_count();
        let _ = mgr.lock().unwrap().stop_streams();

        // MEETILY_W93_EXPECT: what the run must demonstrate. Unset or `prefix`
        // (no recovery task) makes no claim; it is the baseline shape.
        let outcomes = outcomes.lock().unwrap().clone();
        match expect.as_str() {
            "flood-stop" => {
                assert!(failure_seen.load(Ordering::SeqCst), "the kill must produce a dead-stream event");
                assert!(outcomes.iter().any(|o| o.starts_with("Exhausted { attempts: 0")), "flood-stop tears down without reopening: {outcomes:?}");
                assert_eq!(callbacks.load(Ordering::SeqCst), 1, "exactly one announcement");
                assert_eq!(end_streams, 0, "the dead stream is gone; nothing keeps looping");
                assert!(state.is_recording(), "the session is not stopped by the stream loss");
            }
            "recover" => {
                assert!(failure_seen.load(Ordering::SeqCst), "the kill must produce a dead-stream event");
                assert!(outcomes.iter().any(|o| o.starts_with("Recovered")), "recovery must succeed: {outcomes:?}");
                assert_eq!(callbacks.load(Ordering::SeqCst), 0, "a recovered stream must not announce an error");
                assert!(state.is_recording(), "the session must keep recording");
                assert_eq!(end_streams, 1, "the stream is back in its slot");
                let n = chunk_history.len();
                assert!(n >= 5, "observe at least 5 s");
                assert!(
                    chunk_history[n - 1] - chunk_history[n - 4] >= 20,
                    "capture must be flowing again in the last 3 s: {:?}",
                    &chunk_history[n - 4..]
                );
            }
            "exhaust" => {
                assert!(failure_seen.load(Ordering::SeqCst), "the kill must produce a dead-stream event");
                assert!(outcomes.iter().any(|o| o.starts_with("Exhausted")), "recovery must give up: {outcomes:?}");
                assert!(callbacks.load(Ordering::SeqCst) >= 1, "giving up must announce once");
                assert!(callbacks.load(Ordering::SeqCst) <= 2, "bounded announcements");
                assert_eq!(end_streams, 0, "the dead stream is gone; nothing keeps looping");
            }
            _ => {}
        }
    }
}
