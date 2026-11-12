//! Per-stream error gate: turns a backend's error callback storm into one
//! log line per interval and exactly one "this stream is dead" escalation.
//!
//! Why this exists (W93, 2026-11-12): when PipeWire was restarted under a live
//! recording, cpal's ALSA worker for the `system_monitor` PCM kept failing
//! `snd_pcm_poll_descriptors_revents` and re-invoked the stream's error
//! callback in a tight loop, about 7,000 times a second. Every call logged four
//! lines and re-ran `stop_recording`, so the journal took ~20k lines a minute
//! and hid every other service. DO NOT log per error callback: log through
//! `record_error` and obey its verdict.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Consecutive errors, with no audio delivered in between, before a stream is
/// declared dead. A lone xrun-style error on a healthy stream never gets here.
pub const ESCALATE_AFTER_CONSECUTIVE: u32 = 3;

/// At most one detailed log line per this interval, per stream.
pub const LOG_INTERVAL: Duration = Duration::from_secs(5);

const NEVER: u64 = u64::MAX;

/// What the caller should do about one error callback.
#[derive(Debug, PartialEq, Eq)]
pub struct ErrorVerdict {
    /// `Some(n)`: log this error in full, noting `n` identical errors were
    /// suppressed since the last log line. `None`: stay silent.
    pub log: Option<u64>,
    /// True exactly once per stream: the stream is dead and needs reopening.
    pub escalate: bool,
    /// Consecutive errors seen without audio in between.
    pub consecutive: u32,
    /// This stream had ALREADY been declared dead before this error
    /// (escalation fired on an earlier call). Terminal for the stream:
    /// recovery owns it from here on.
    pub terminal: bool,
}

impl ErrorVerdict {
    /// Should this error be fed to `RecordingState::report_error` (the user
    /// message and the session-stop counters)?
    ///
    /// Only a FATAL class (permission denied, channel closed) is, immediately,
    /// regardless of the log rate limit, so the limiter can never swallow it.
    /// A recoverable error is only logged: it either escalates to the recovery
    /// task, which reopens the stream and tells the user itself if it gives
    /// up, or it was transient. Announcing it first would toast an error for
    /// a stream that is about to be silently reopened.
    ///
    /// Never after escalation (`terminal`): escalation is terminal for that
    /// stream, so a dying stream's tail errors cannot stop the session. The
    /// escalating call itself is not yet terminal, so a fatal error arriving
    /// as the third consecutive one is still reported.
    pub fn reports_to_session(&self, fatal_class: bool) -> bool {
        fatal_class && !self.terminal
    }
}

pub struct StreamErrorGate {
    epoch: Instant,
    consecutive: AtomicU32,
    escalated: AtomicBool,
    suppressed: AtomicU64,
    last_log_ms: AtomicU64,
}

impl StreamErrorGate {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            consecutive: AtomicU32::new(0),
            escalated: AtomicBool::new(false),
            suppressed: AtomicU64::new(0),
            last_log_ms: AtomicU64::new(NEVER),
        }
    }

    /// Audio arrived, so the stream is alive: a lone earlier error was transient.
    /// Called from the capture data callback; one relaxed store, no allocation.
    pub fn note_data(&self) {
        self.consecutive.store(0, Ordering::Relaxed);
    }

    /// True once this stream has been declared dead (escalation already fired).
    pub fn escalated(&self) -> bool {
        self.escalated.load(Ordering::SeqCst)
    }

    pub fn record_error(&self) -> ErrorVerdict {
        self.record_error_at(Instant::now())
    }

    pub fn record_error_at(&self, now: Instant) -> ErrorVerdict {
        let terminal = self.escalated.load(Ordering::SeqCst);
        let consecutive = self.consecutive.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        let escalate = consecutive >= ESCALATE_AFTER_CONSECUTIVE
            && !self.escalated.swap(true, Ordering::SeqCst);

        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        let last = self.last_log_ms.load(Ordering::Relaxed);
        let due = last == NEVER || now_ms.saturating_sub(last) >= LOG_INTERVAL.as_millis() as u64;
        let log = if due
            && self
                .last_log_ms
                .compare_exchange(last, now_ms, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            Some(self.suppressed.swap(0, Ordering::SeqCst))
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
            None
        };

        ErrorVerdict {
            log,
            escalate,
            consecutive,
            terminal,
        }
    }
}

impl Default for StreamErrorGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storm_logs_once_per_interval_and_escalates_once() {
        let gate = StreamErrorGate::new();
        let t0 = Instant::now();
        let mut logged = 0;
        let mut escalated = 0;
        let mut suppressed_reported = 0;
        // 7,000 errors/s for 12 seconds, the rate measured in W93.
        for i in 0..(7_000u64 * 12) {
            let now = t0 + Duration::from_micros(i * 1_000_000 / 7_000);
            let v = gate.record_error_at(now);
            if let Some(n) = v.log {
                logged += 1;
                suppressed_reported += n;
            }
            if v.escalate {
                escalated += 1;
            }
        }
        // t=0, 5s, 10s: three lines for twelve seconds of storm, not 84,000.
        assert_eq!(logged, 3, "one line per 5s interval");
        assert_eq!(escalated, 1, "dead-stream escalation fires once per stream");
        // Nothing is lost silently: every error is either logged or counted.
        assert_eq!(suppressed_reported + logged as u64 + gate.suppressed.load(Ordering::SeqCst), 7_000 * 12);
    }

    #[test]
    fn isolated_errors_between_audio_never_escalate() {
        let gate = StreamErrorGate::new();
        let t0 = Instant::now();
        for i in 0..100 {
            let v = gate.record_error_at(t0 + Duration::from_secs(i));
            assert!(!v.escalate);
            gate.note_data();
        }
    }

    #[test]
    fn errors_resume_logging_after_the_interval() {
        let gate = StreamErrorGate::new();
        let t0 = Instant::now();
        assert_eq!(gate.record_error_at(t0).log, Some(0));
        assert_eq!(gate.record_error_at(t0 + Duration::from_secs(1)).log, None);
        assert_eq!(gate.record_error_at(t0 + Duration::from_secs(2)).log, None);
        assert_eq!(gate.record_error_at(t0 + Duration::from_secs(6)).log, Some(2));
    }

    /// Once a stream is declared dead its tail errors must not reach the
    /// session-stop counters, however long its teardown takes; before that,
    /// recoverable errors are only logged.
    #[test]
    fn escalation_is_terminal_for_session_reporting() {
        let gate = StreamErrorGate::new();
        let t0 = Instant::now();
        let (mut reported_recoverable, mut reported_fatal) = (0, 0);
        // 15 s of a 7,000/s storm.
        for i in 0..(7_000u64 * 15) {
            let v = gate.record_error_at(t0 + Duration::from_micros(i * 1_000_000 / 7_000));
            reported_recoverable += usize::from(v.reports_to_session(false));
            reported_fatal += usize::from(v.reports_to_session(true));
        }
        assert_eq!(reported_recoverable, 0, "recoverable errors are never announced by the gate");
        assert_eq!(reported_fatal, 3, "fatal reports stop at escalation: calls 1-3 only (call 3 escalates, not yet terminal)");
    }

    /// A fatal error is reported even inside the log quiet window, and also
    /// when it is the very call that escalates.
    #[test]
    fn a_fatal_class_is_never_swallowed_before_the_stream_is_terminal() {
        let gate = StreamErrorGate::new();
        let t0 = Instant::now();
        let first = gate.record_error_at(t0);
        assert!(first.log.is_some());
        let quiet = gate.record_error_at(t0 + Duration::from_secs(1));
        assert!(quiet.log.is_none(), "inside the quiet window");
        assert!(quiet.reports_to_session(true), "fatal bypasses the limiter");
        let escalating = gate.record_error_at(t0 + Duration::from_secs(2));
        assert!(escalating.escalate);
        assert!(escalating.reports_to_session(true), "the escalating call is not terminal yet");
        let tail = gate.record_error_at(t0 + Duration::from_secs(3));
        assert!(!tail.reports_to_session(true), "terminal afterwards");
    }

    #[test]
    fn recoverable_errors_are_logged_not_announced() {
        let gate = StreamErrorGate::new();
        let v = gate.record_error_at(Instant::now());
        assert!(v.log.is_some());
        assert!(!v.reports_to_session(false));
    }

    #[test]
    fn escalates_after_threshold_not_before() {
        let gate = StreamErrorGate::new();
        let t0 = Instant::now();
        assert!(!gate.record_error_at(t0).escalate);
        assert!(!gate.record_error_at(t0).escalate);
        assert!(gate.record_error_at(t0).escalate);
        assert!(!gate.record_error_at(t0).escalate);
    }
}
