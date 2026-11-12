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

    /// True if this stream was already declared dead. A replacement can die
    /// before it reaches its slot, when its failure event is discarded as
    /// stale; the recovery run re-checks this after installing it.
    pub fn is_dead(&self) -> bool {
        self.capture.is_dead()
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

/// Audio stream manager for handling multiple streams
pub struct AudioStreamManager {
    microphone_stream: Option<AudioStream>,
    system_stream: Option<AudioStream>,
    state: Arc<RecordingState>,
}

// SAFETY: AudioStreamManager contains AudioStream which we've marked as Send
unsafe impl Send for AudioStreamManager {}

impl AudioStreamManager {
    pub fn new(state: Arc<RecordingState>) -> Self {
        Self {
            microphone_stream: None,
            system_stream: None,
            state,
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
                    self.microphone_stream = Some(stream);
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
                    self.system_stream = Some(stream);
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
        if self.microphone_stream.is_none() && self.system_stream.is_none() {
            return Err(anyhow::anyhow!("No audio streams could be created"));
        }

        Ok(())
    }

    /// Stop all audio streams
    pub fn stop_streams(&mut self) -> Result<()> {
        info!("Stopping all audio streams");

        let mut errors = Vec::new();

        // Stop microphone stream
        if let Some(mic_stream) = self.microphone_stream.take() {
            if let Err(e) = mic_stream.stop() {
                error!("Failed to stop microphone stream: {}", e);
                errors.push(e);
            }
        }

        // Stop system stream
        if let Some(sys_stream) = self.system_stream.take() {
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

    /// Take the microphone stream OUT of the manager, keeping system audio
    /// running. The caller stops/drops it OUTSIDE any lock — a cpal teardown
    /// of a dead BT device can stall and must not block a held mutex.
    pub fn take_mic_stream(&mut self) -> Option<AudioStream> {
        self.microphone_stream.take()
    }

    /// Set a new microphone stream (used after hot-swap creation). Returns the
    /// stream it replaced, if a dead-stream recovery had installed one
    /// meanwhile: the caller must tear that one down OUTSIDE the manager lock
    /// (a cpal drop joins its worker and can stall).
    pub fn set_mic_stream(&mut self, stream: AudioStream) -> Option<AudioStream> {
        self.microphone_stream.replace(stream)
    }

    /// Take the stream for `kind` out of its slot (dead-stream recovery), but
    /// only if it is still the stream that failed (`stream_id`). A stale event
    /// from a stream that was already replaced (mic hot-swap) must not tear
    /// down its healthy successor. The caller tears the result down OUTSIDE
    /// any lock, like `take_mic_stream`.
    pub fn take_stream_if_current(&mut self, kind: DeviceType, stream_id: u64) -> Option<AudioStream> {
        let slot = match kind {
            DeviceType::Microphone => &mut self.microphone_stream,
            DeviceType::System => &mut self.system_stream,
        };
        if slot.as_ref().map(AudioStream::capture_id) == Some(stream_id) {
            slot.take()
        } else {
            None
        }
    }

    /// Whether `kind`'s slot currently holds a stream.
    pub fn has_stream(&self, kind: DeviceType) -> bool {
        match kind {
            DeviceType::Microphone => self.microphone_stream.is_some(),
            DeviceType::System => self.system_stream.is_some(),
        }
    }

    /// Id of the stream in `kind`'s slot if it has already been declared dead.
    pub fn dead_stream_id(&self, kind: DeviceType) -> Option<u64> {
        let slot = match kind {
            DeviceType::Microphone => &self.microphone_stream,
            DeviceType::System => &self.system_stream,
        };
        slot.as_ref().filter(|s| s.is_dead()).map(AudioStream::capture_id)
    }

    /// Install a recovered stream only if its slot is still empty. Returns the
    /// stream back when something else (a mic hot-swap) refilled the slot.
    pub fn install_stream_if_empty(&mut self, kind: DeviceType, stream: AudioStream) -> Option<AudioStream> {
        let slot = match kind {
            DeviceType::Microphone => &mut self.microphone_stream,
            DeviceType::System => &mut self.system_stream,
        };
        if slot.is_some() {
            return Some(stream);
        }
        *slot = Some(stream);
        None
    }

    /// Get stream count
    pub fn active_stream_count(&self) -> usize {
        let mut count = 0;
        if self.microphone_stream.is_some() {
            count += 1;
        }
        if self.system_stream.is_some() {
            count += 1;
        }
        count
    }

    /// Check if any streams are active
    pub fn has_active_streams(&self) -> bool {
        self.microphone_stream.is_some() || self.system_stream.is_some()
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
        if recover {
            let mut failures = state.take_stream_failure_receiver().unwrap();
            let (state2, mgr2, outcomes2, dev2) = (state.clone(), mgr.clone(), outcomes.clone(), dev.clone());
            tokio::spawn(async move {
                while let Some(crate::audio::recording_state::StreamFailure { kind, stream_id }) = failures.recv().await {
                    let dead = mgr2.lock().unwrap().take_stream_if_current(kind, stream_id);
                    if dead.is_none() {
                        eprintln!("W93_STALE failure for stream {stream_id}");
                        continue;
                    }
                    let out = recover_stream(
                        kind,
                        RecoveryPolicy::default(),
                        move || dead,
                        |s: AudioStream| async move {
                            let _ = tokio::task::spawn_blocking(move || {
                                let _ = s.stop();
                            })
                            .await;
                        },
                        || {
                            let (st, d) = (state2.clone(), dev2.clone());
                            async move { AudioStream::create(d, st, kind, None).await }
                        },
                        |s| mgr2.lock().unwrap().install_stream_if_empty(kind, s),
                        || state2.is_recording(),
                        tokio::time::sleep,
                    )
                    .await;
                    eprintln!("W93_OUTCOME {:?}", out);
                    outcomes2.lock().unwrap().push(format!("{:?}", out));
                    if let RecoveryOutcome::Exhausted { .. } = out {
                        state2.report_error(crate::audio::recording_state::AudioError::DeviceDisconnected);
                    }
                }
            });
        }

        for s in 0..observe_secs {
            tokio::time::sleep(Duration::from_secs(1)).await;
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
        let _ = mgr.lock().unwrap().stop_streams();
    }
}
