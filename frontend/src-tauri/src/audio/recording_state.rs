use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;
use anyhow::Result;

use super::devices::AudioDevice;
use super::buffer_pool::AudioBufferPool;

/// Device type for audio chunks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Microphone,
    System,
}

/// A capture stream reported dead. `stream_id` identifies WHICH stream (see
/// `AudioCapture::stream_id`) so recovery never tears down a healthy
/// replacement when a stale event from an already-replaced stream arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFailure {
    pub kind: DeviceType,
    pub stream_id: u64,
}

/// Audio chunk with metadata for processing
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub data: Vec<f32>,
    pub sample_rate: u32,
    pub timestamp: f64,
    pub chunk_id: u64,
    pub device_type: DeviceType,
    /// What `data` holds, and therefore how it may be labelled downstream.
    /// Never derive durations from `data.len()` alone: divide by
    /// `layout.channels()` to get frames.
    pub layout: AudioLayout,
}

/// Channel layout and source meaning of an `AudioChunk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioLayout {
    /// One channel with no single source (a mix, or a raw per-device stream
    /// before mixing). Transcripts of it carry no source label.
    Mono,
    /// One channel that is the microphone alone (no system audio exists).
    MicOnly,
    /// Two interleaved channels, frame = [mic, system].
    MicSystemStereo,
}

impl AudioLayout {
    pub fn channels(self) -> u16 {
        match self {
            AudioLayout::Mono | AudioLayout::MicOnly => 1,
            AudioLayout::MicSystemStereo => 2,
        }
    }
}

/// Processed audio chunk (post-VAD) for recording
#[derive(Debug, Clone)]
pub struct ProcessedAudioChunk {
    pub data: Vec<f32>,
    pub sample_rate: u32,
    pub timestamp: f64,
    pub device_type: DeviceType,
}

/// Comprehensive error types for audio system
#[derive(Debug, Clone)]
pub enum AudioError {
    DeviceDisconnected,
    StreamFailed,
    ProcessingFailed,
    TranscriptionFailed,
    ChannelClosed,
    InitializationFailed,
    ConfigurationError,
    PermissionDenied,
    BufferOverflow,
    SampleRateUnsupported,
}

impl AudioError {
    /// Check if error is recoverable (can attempt reconnection)
    pub fn is_recoverable(&self) -> bool {
        match self {
            // Device disconnect is now recoverable - we can attempt reconnection
            AudioError::DeviceDisconnected => true,
            AudioError::StreamFailed => true,
            AudioError::ProcessingFailed => true,
            AudioError::TranscriptionFailed => true,
            AudioError::ChannelClosed => false,
            AudioError::InitializationFailed => false,
            AudioError::ConfigurationError => false,
            AudioError::PermissionDenied => false,
            AudioError::BufferOverflow => true,
            AudioError::SampleRateUnsupported => false,
        }
    }

    /// Get user-friendly error message
    pub fn user_message(&self) -> &'static str {
        match self {
            AudioError::DeviceDisconnected => "Audio device was disconnected",
            AudioError::StreamFailed => "Audio stream encountered an error",
            AudioError::ProcessingFailed => "Audio processing failed",
            AudioError::TranscriptionFailed => "Speech transcription failed",
            AudioError::ChannelClosed => "Audio channel was closed unexpectedly",
            AudioError::InitializationFailed => "Failed to initialize audio system",
            AudioError::ConfigurationError => "Audio configuration error",
            AudioError::PermissionDenied => "Microphone permission denied",
            AudioError::BufferOverflow => "Audio buffer overflow",
            AudioError::SampleRateUnsupported => "Audio sample rate not supported",
        }
    }
}

/// Recording statistics
#[derive(Debug, Default)]
pub struct RecordingStats {
    pub chunks_processed: u64,
    pub total_duration: f64,
    pub last_activity: Option<Instant>,
}

/// Unified state management for audio recording
pub struct RecordingState {
    // Core recording state
    is_recording: AtomicBool,
    is_paused: AtomicBool,

    // Audio devices
    microphone_device: Mutex<Option<Arc<AudioDevice>>>,
    system_device: Mutex<Option<Arc<AudioDevice>>>,

    // Audio pipeline
    audio_sender: Mutex<Option<mpsc::UnboundedSender<AudioChunk>>>,

    // Memory optimization
    buffer_pool: AudioBufferPool,

    // Error handling
    error_count: AtomicU32,
    recoverable_error_count: AtomicU32,
    last_error: Mutex<Option<AudioError>>,
    error_callback: Mutex<Option<Box<dyn Fn(&AudioError) + Send + Sync>>>,

    // Dead-stream notifications. A capture whose backend keeps erroring reports
    // here ONCE per stream (see `StreamErrorGate`); the recovery task owns the
    // receiver and reopens the stream with bounded backoff.
    stream_failure_tx: mpsc::UnboundedSender<StreamFailure>,
    stream_failure_rx: Mutex<Option<mpsc::UnboundedReceiver<StreamFailure>>>,

    // Statistics
    stats: Mutex<RecordingStats>,

    // Recording start time for accurate timestamps
    recording_start: Mutex<Option<Instant>>,
    // Pause time tracking
    pause_start: Mutex<Option<Instant>>,
    total_pause_duration: Mutex<std::time::Duration>,
}

impl RecordingState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    // Recording control
    pub fn start_recording(&self) -> Result<()> {
        self.is_recording.store(true, Ordering::SeqCst);
        *self.recording_start.lock().unwrap() = Some(Instant::now());
        self.error_count.store(0, Ordering::SeqCst);
        self.recoverable_error_count.store(0, Ordering::SeqCst);
        *self.last_error.lock().unwrap() = None;
        Ok(())
    }

    pub fn stop_recording(&self) {
        self.is_recording.store(false, Ordering::SeqCst);
        self.is_paused.store(false, Ordering::SeqCst);
        // Clear pause tracking when stopping
        *self.pause_start.lock().unwrap() = None;
        // CRITICAL: Clear audio sender to close the pipeline channel
        // This ensures the pipeline loop exits properly after processing all chunks
        *self.audio_sender.lock().unwrap() = None;
        // CRITICAL: Clear device references to release microphone/speaker
        // Without this, Arc<AudioDevice> references persist and keep the mic active
        *self.microphone_device.lock().unwrap() = None;
        *self.system_device.lock().unwrap() = None;
        log::info!("Recording stopped, device references cleared");
    }

    pub fn pause_recording(&self) -> Result<()> {
        if !self.is_recording() {
            return Err(anyhow::anyhow!("Cannot pause when not recording"));
        }
        if self.is_paused() {
            return Err(anyhow::anyhow!("Recording is already paused"));
        }

        self.is_paused.store(true, Ordering::SeqCst);
        *self.pause_start.lock().unwrap() = Some(Instant::now());
        log::info!("Recording paused");
        Ok(())
    }

    pub fn resume_recording(&self) -> Result<()> {
        if !self.is_recording() {
            return Err(anyhow::anyhow!("Cannot resume when not recording"));
        }
        if !self.is_paused() {
            return Err(anyhow::anyhow!("Recording is not paused"));
        }

        // Calculate pause duration and add to total
        if let Some(pause_start) = self.pause_start.lock().unwrap().take() {
            let pause_duration = pause_start.elapsed();
            *self.total_pause_duration.lock().unwrap() += pause_duration;
            log::info!("Recording resumed after pause of {:.2}s", pause_duration.as_secs_f64());
        }

        self.is_paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        self.is_recording.load(Ordering::SeqCst)
    }

    pub fn is_paused(&self) -> bool {
        self.is_paused.load(Ordering::SeqCst)
    }

    pub fn is_active(&self) -> bool {
        self.is_recording() && !self.is_paused()
    }

    // Device management
    pub fn set_microphone_device(&self, device: Arc<AudioDevice>) {
        *self.microphone_device.lock().unwrap() = Some(device);
    }

    pub fn set_system_device(&self, device: Arc<AudioDevice>) {
        *self.system_device.lock().unwrap() = Some(device);
    }

    pub fn get_microphone_device(&self) -> Option<Arc<AudioDevice>> {
        self.microphone_device.lock().unwrap().clone()
    }

    pub fn get_system_device(&self) -> Option<Arc<AudioDevice>> {
        self.system_device.lock().unwrap().clone()
    }

    // Audio pipeline management
    pub fn set_audio_sender(&self, sender: mpsc::UnboundedSender<AudioChunk>) {
        *self.audio_sender.lock().unwrap() = Some(sender);
    }

    pub fn send_audio_chunk(&self, chunk: AudioChunk) -> Result<()> {
        // Don't send audio chunks when paused
        if self.is_paused() {
            return Ok(()); // Silently discard chunks while paused
        }

        if let Some(sender) = self.audio_sender.lock().unwrap().as_ref() {
            sender.send(chunk).map_err(|_| anyhow::anyhow!("Failed to send audio chunk"))?;

            // Update statistics
            let mut stats = self.stats.lock().unwrap();
            stats.chunks_processed += 1;
            stats.last_activity = Some(Instant::now());
            Ok(())
        } else {
            // Return an error when no sender is available (pipeline not ready)
            Err(anyhow::anyhow!("Audio pipeline not ready - no sender available"))
        }
    }

    // Error handling
    pub fn set_error_callback<F>(&self, callback: F)
    where
        F: Fn(&AudioError) + Send + Sync + 'static,
    {
        *self.error_callback.lock().unwrap() = Some(Box::new(callback));
    }

    pub fn report_error(&self, error: AudioError) {
        let count = self.error_count.fetch_add(1, Ordering::SeqCst) + 1;

        // Once recording has stopped there is nothing left to stop or announce.
        // A dead backend stream used to call this thousands of times a second,
        // and each call re-logged and re-ran `stop_recording` (W93: the journal
        // flood after a PipeWire restart). Count it, keep the first error for
        // diagnostics, and stay quiet.
        if !self.is_recording() {
            log::debug!("Audio error after recording stopped (#{}): {:?}", count, error);
            return;
        }

        // Track recoverable vs non-recoverable errors separately
        if error.is_recoverable() {
            let recoverable_count = self.recoverable_error_count.fetch_add(1, Ordering::SeqCst) + 1;
            log::warn!("Recoverable audio error ({}): {:?}", recoverable_count, error);

            // Allow more recoverable errors before stopping
            if recoverable_count >= 10 {
                log::error!("Too many recoverable errors ({}), stopping recording", recoverable_count);
                self.stop_recording();
            }
        } else {
            log::error!("Non-recoverable audio error: {:?}", error);
            // Stop immediately for non-recoverable errors
            self.stop_recording();
        }

        *self.last_error.lock().unwrap() = Some(error.clone());

        // Call error callback if set
        if let Some(callback) = self.error_callback.lock().unwrap().as_ref() {
            callback(&error);
        }

        // Fallback: stop recording after too many total errors
        if count >= 15 && self.is_recording() {
            log::error!("Too many total audio errors ({}), stopping recording", count);
            self.stop_recording();
        }
    }

    /// Tell the user about an error WITHOUT touching the stop counters: used
    /// when recovery gives up on one stream, which must not stop a session
    /// whose other stream is fine (and must not be counted toward the
    /// "10 recoverable errors" stop either).
    pub fn announce_error(&self, error: AudioError) {
        *self.last_error.lock().unwrap() = Some(error.clone());
        if let Some(callback) = self.error_callback.lock().unwrap().as_ref() {
            callback(&error);
        }
    }

    /// A capture stream is dead: its backend keeps erroring with no audio in
    /// between. Fired at most once per stream (`StreamErrorGate`); the recovery
    /// task reopens the stream. No-op once the receiver is gone.
    pub fn report_stream_failure(&self, failure: StreamFailure) {
        let _ = self.stream_failure_tx.send(failure);
    }

    /// Hand the failure receiver to the recovery task. `None` after the first call.
    pub fn take_stream_failure_receiver(&self) -> Option<mpsc::UnboundedReceiver<StreamFailure>> {
        self.stream_failure_rx.lock().unwrap().take()
    }

    pub fn get_error_count(&self) -> u32 {
        self.error_count.load(Ordering::SeqCst)
    }

    pub fn get_recoverable_error_count(&self) -> u32 {
        self.recoverable_error_count.load(Ordering::SeqCst)
    }

    pub fn get_last_error(&self) -> Option<AudioError> {
        self.last_error.lock().unwrap().clone()
    }

    pub fn has_fatal_error(&self) -> bool {
        if let Some(error) = &*self.last_error.lock().unwrap() {
            !error.is_recoverable() && self.error_count.load(Ordering::SeqCst) > 0
        } else {
            false
        }
    }

    // Statistics
    pub fn get_stats(&self) -> RecordingStats {
        self.stats.lock().unwrap().clone()
    }

    pub fn get_recording_duration(&self) -> Option<f64> {
        self.recording_start
            .lock()
            .unwrap()
            .map(|start| start.elapsed().as_secs_f64())
    }

    pub fn get_active_recording_duration(&self) -> Option<f64> {
        self.recording_start.lock().unwrap().map(|start| {
            let total_duration = start.elapsed().as_secs_f64();
            let pause_duration = self.get_total_pause_duration();
            let current_pause = if self.is_paused() {
                self.pause_start
                    .lock()
                    .unwrap()
                    .map(|p| p.elapsed().as_secs_f64())
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            total_duration - pause_duration - current_pause
        })
    }

    pub fn get_total_pause_duration(&self) -> f64 {
        self.total_pause_duration.lock().unwrap().as_secs_f64()
    }

    pub fn get_current_pause_duration(&self) -> Option<f64> {
        if self.is_paused() {
            self.pause_start
                .lock()
                .unwrap()
                .map(|start| start.elapsed().as_secs_f64())
        } else {
            None
        }
    }

    // Memory management
    pub fn get_buffer_pool(&self) -> AudioBufferPool {
        self.buffer_pool.clone()
    }

    // Cleanup
    pub fn cleanup(&self) {
        self.stop_recording();
        *self.microphone_device.lock().unwrap() = None;
        *self.system_device.lock().unwrap() = None;
        *self.audio_sender.lock().unwrap() = None;
        *self.last_error.lock().unwrap() = None;
        *self.error_callback.lock().unwrap() = None;
        *self.stats.lock().unwrap() = RecordingStats::default();
        *self.recording_start.lock().unwrap() = None;
        *self.pause_start.lock().unwrap() = None;
        *self.total_pause_duration.lock().unwrap() = std::time::Duration::ZERO;
        self.error_count.store(0, Ordering::SeqCst);
        self.recoverable_error_count.store(0, Ordering::SeqCst);

        // Clear buffer pool to free memory
        self.buffer_pool.clear();
    }
}

impl Default for RecordingState {
    fn default() -> Self {
        let (stream_failure_tx, stream_failure_rx) = mpsc::unbounded_channel();
        Self {
            is_recording: AtomicBool::new(false),
            is_paused: AtomicBool::new(false),
            microphone_device: Mutex::new(None),
            system_device: Mutex::new(None),
            audio_sender: Mutex::new(None),
            buffer_pool: AudioBufferPool::new(16, 48000), // Pool of 16 buffers with 48kHz samples capacity
            error_count: AtomicU32::new(0),
            recoverable_error_count: AtomicU32::new(0),
            last_error: Mutex::new(None),
            error_callback: Mutex::new(None),
            stream_failure_tx,
            stream_failure_rx: Mutex::new(Some(stream_failure_rx)),
            stats: Mutex::new(RecordingStats::default()),
            recording_start: Mutex::new(None),
            pause_start: Mutex::new(None),
            total_pause_duration: Mutex::new(std::time::Duration::ZERO),
        }
    }
}

// Thread-safe cloning for RecordingStats
impl Clone for RecordingStats {
    fn clone(&self) -> Self {
        Self {
            chunks_processed: self.chunks_processed,
            total_duration: self.total_duration,
            last_activity: self.last_activity,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn counting_state() -> (Arc<RecordingState>, Arc<AtomicUsize>) {
        let state = RecordingState::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        state.set_error_callback(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        state.start_recording().unwrap();
        (state, calls)
    }

    /// W93: a dead backend stream reported thousands of errors a second and each
    /// one re-ran stop_recording and the user callback. After the session has
    /// stopped, further errors are counted but must be inert.
    #[test]
    fn errors_after_stop_are_counted_but_inert() {
        let (state, calls) = counting_state();
        for _ in 0..10 {
            state.report_error(AudioError::StreamFailed);
        }
        assert!(!state.is_recording(), "10 recoverable errors stop the session");
        let callbacks_at_stop = calls.load(Ordering::SeqCst);
        assert_eq!(callbacks_at_stop, 10);

        for _ in 0..50_000 {
            state.report_error(AudioError::StreamFailed);
        }
        assert_eq!(calls.load(Ordering::SeqCst), callbacks_at_stop, "no callback after stop");
        assert_eq!(state.get_error_count(), 50_010, "still counted");
        assert_eq!(state.get_recoverable_error_count(), 10, "recoverable budget not inflated");
    }

    #[test]
    fn non_recoverable_error_stops_once() {
        let (state, calls) = counting_state();
        state.report_error(AudioError::InitializationFailed);
        state.report_error(AudioError::InitializationFailed);
        assert!(!state.is_recording());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stream_failure_receiver_is_handed_out_once() {
        let state = RecordingState::new();
        let mut rx = state.take_stream_failure_receiver().expect("first take");
        assert!(state.take_stream_failure_receiver().is_none());
        let f = StreamFailure { kind: DeviceType::System, stream_id: 7 };
        state.report_stream_failure(f);
        assert_eq!(rx.try_recv().unwrap(), f);
    }
}
