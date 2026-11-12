//! Bounded reopen of a dead capture stream.
//!
//! A capture stream whose backend keeps failing (PipeWire restarted, device
//! unplugged) cannot be fixed by flipping `RecordingState` flags: the cpal
//! worker thread keeps spinning until the `Stream` is dropped. Recovery
//! therefore (1) takes the dead stream out and drops it so the error loop
//! ends, then (2) reopens the same device with exponential backoff, a bounded
//! number of times, and (3) gives up loudly (one error, no loop) if the device
//! never comes back. DO NOT retry without backoff or without a cap: that is
//! the unbounded loop this module replaces (W93).

use std::future::Future;
use std::time::{Duration, Instant};

use log::{info, warn};

use super::recording_state::DeviceType;

/// Retry schedule for one recovery run.
#[derive(Debug, Clone, Copy)]
pub struct RecoveryPolicy {
    /// Reopen attempts per run before giving up.
    pub max_attempts: u32,
    /// Delay before the first attempt; doubles each attempt.
    pub base_delay: Duration,
    /// Cap on the doubled delay.
    pub max_delay: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(16),
        }
    }
}

impl RecoveryPolicy {
    /// Delay slept before 1-based `attempt`: base, 2*base, 4*base ... capped.
    pub fn delay_before(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        self.base_delay
            .checked_mul(1u32 << shift)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// A new stream is installed and capturing.
    Recovered { attempts: u32 },
    /// Every attempt failed; the dead stream is already gone, nothing loops.
    Exhausted { attempts: u32 },
    /// The recording ended or the stream slot was taken by something else.
    Abandoned,
}

/// Tear down the dead stream, then reopen with backoff.
///
/// Generic over the stream type `S` so the policy is unit-testable without
/// audio hardware. Production passes `AudioStream`.
///
/// * `take_dead` removes the failed stream from its slot (None if already gone).
/// * `teardown` drops it; it must not run under any recording lock.
/// * `open` builds a fresh stream on the same device.
/// * `install` puts it in the slot, handing it back if the slot is no longer
///   ours (a mic hot-swap got there first, or the session changed).
/// * `live` is true while this recovery is still wanted: the session is
///   recording AND the stream's slot is still empty. A mic hot-swap that
///   filled the slot meanwhile makes it false, so a stale run neither retries
///   against a healthy replacement nor announces a failure that did not happen.
pub async fn recover_stream<S, TakeFn, TearFut, TearFn, OpenFn, OpenFut, InstFn, LiveFn, SleepFn, SleepFut>(
    kind: DeviceType,
    policy: RecoveryPolicy,
    take_dead: TakeFn,
    teardown: TearFn,
    open: OpenFn,
    install: InstFn,
    live: LiveFn,
    sleep: SleepFn,
) -> RecoveryOutcome
where
    TakeFn: FnOnce() -> Option<S>,
    TearFn: Fn(S) -> TearFut,
    TearFut: Future<Output = ()>,
    OpenFn: Fn() -> OpenFut,
    OpenFut: Future<Output = anyhow::Result<S>>,
    InstFn: Fn(S) -> Option<S>,
    LiveFn: Fn() -> bool,
    SleepFn: Fn(Duration) -> SleepFut,
    SleepFut: Future<Output = ()>,
{
    if let Some(dead) = take_dead() {
        teardown(dead).await;
        info!("[STREAM_RECOVERY] {:?}: dead stream torn down", kind);
    }

    for attempt in 1..=policy.max_attempts {
        sleep(policy.delay_before(attempt)).await;

        if !live() {
            info!("[STREAM_RECOVERY] {:?}: recording ended, abandoning", kind);
            return RecoveryOutcome::Abandoned;
        }

        match open().await {
            Ok(stream) => {
                if !live() {
                    teardown(stream).await;
                    return RecoveryOutcome::Abandoned;
                }
                match install(stream) {
                    None => {
                        info!("[STREAM_RECOVERY] {:?}: reopened on attempt {}", kind, attempt);
                        return RecoveryOutcome::Recovered { attempts: attempt };
                    }
                    Some(rejected) => {
                        teardown(rejected).await;
                        info!("[STREAM_RECOVERY] {:?}: slot taken meanwhile, abandoning", kind);
                        return RecoveryOutcome::Abandoned;
                    }
                }
            }
            Err(e) => {
                warn!(
                    "[STREAM_RECOVERY] {:?}: reopen attempt {}/{} failed: {}",
                    kind, attempt, policy.max_attempts, e
                );
            }
        }
    }

    // The recording may have ended during the last attempt; that is not a
    // device failure worth announcing.
    if !live() {
        return RecoveryOutcome::Abandoned;
    }
    RecoveryOutcome::Exhausted { attempts: policy.max_attempts }
}

/// Run `stop` (a cpal stream stop/drop, which joins the backend worker and can
/// stall forever on a dead device) off the async workers and wait at most
/// `bound`. Returns true if it timed out. The blocking thread cannot be
/// cancelled and keeps only that one stream alive; waiting on it unbounded
/// would instead hang the caller, and with it everything the caller holds
/// (a mic hot-swap holds `MIC_SWAP_IN_PROGRESS`).
pub async fn teardown_bounded<F>(stop: F, bound: Duration) -> bool
where
    F: FnOnce() + Send + 'static,
{
    let done = tokio::task::spawn_blocking(stop);
    tokio::time::timeout(bound, done).await.is_err()
}

/// A flag held for the lifetime of the guard and cleared on EVERY exit path:
/// return, `?`, panic unwind, task abort, teardown timeout. Used for
/// `MIC_SWAP_IN_PROGRESS`, which must never be left stuck (a stuck flag
/// silently disables every later mic fallback).
pub struct FlagGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl<'a> FlagGuard<'a> {
    /// Take the flag; `None` if it is already held.
    pub fn try_acquire(flag: &'a std::sync::atomic::AtomicBool) -> Option<Self> {
        use std::sync::atomic::Ordering;
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for FlagGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Caps how often a stream may be reopened when each replacement dies again
/// soon after. A recovered stream that stays healthy for `STABLE_AFTER`
/// resets the count.
pub const MAX_RAPID_RECOVERIES: u32 = 3;
pub const STABLE_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct RecoveryLedger {
    rapid: u32,
    last_recovered: Option<Instant>,
}

impl RecoveryLedger {
    /// A stream failed at `now`. True if another recovery run is allowed.
    pub fn allow(&mut self, now: Instant) -> bool {
        if let Some(t) = self.last_recovered {
            if now.saturating_duration_since(t) >= STABLE_AFTER {
                self.rapid = 0;
            }
        }
        self.rapid < MAX_RAPID_RECOVERIES
    }

    /// A run was started (counts against the rapid-failure budget).
    pub fn started(&mut self) {
        self.rapid += 1;
    }

    pub fn recovered(&mut self, now: Instant) {
        self.last_recovered = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn policy() -> RecoveryPolicy {
        RecoveryPolicy { max_attempts: 5, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(8) }
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let p = RecoveryPolicy::default();
        let d: Vec<u64> = (1..=7).map(|a| p.delay_before(a).as_secs()).collect();
        assert_eq!(d, vec![1, 2, 4, 8, 16, 16, 16]);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts_and_tears_down_the_dead_stream_first() {
        let torn = RefCell::new(Vec::new());
        let opens = Cell::new(0u32);
        let sleeps = RefCell::new(Vec::new());
        let out = recover_stream(
            DeviceType::System,
            policy(),
            || Some("dead"),
            |s| { torn.borrow_mut().push(s); async {} },
            || { opens.set(opens.get() + 1); async { Err(anyhow::anyhow!("pipewire not back")) } },
            |s: &str| Some(s),
            || true,
            |d| { sleeps.borrow_mut().push(d); async {} },
        )
        .await;
        assert_eq!(out, RecoveryOutcome::Exhausted { attempts: 5 });
        assert_eq!(opens.get(), 5, "bounded: exactly max_attempts reopen tries");
        assert_eq!(&*torn.borrow(), &["dead"], "dead stream dropped so its error loop ends");
        let ms: Vec<u128> = sleeps.borrow().iter().map(|d| d.as_millis()).collect();
        assert_eq!(ms, vec![1, 2, 4, 8, 8], "exponential backoff between attempts");
    }

    #[tokio::test]
    async fn no_exhaustion_error_if_the_recording_ended_during_the_last_attempt() {
        let checks = Cell::new(0u32);
        let out = recover_stream(
            DeviceType::System,
            RecoveryPolicy { max_attempts: 2, base_delay: Duration::from_millis(1), max_delay: Duration::from_millis(1) },
            || None::<&str>,
            |_| async {},
            || async { Err(anyhow::anyhow!("gone")) },
            |s| Some(s),
            // live for both attempt checks, false once the loop has ended
            || { checks.set(checks.get() + 1); checks.get() <= 2 },
            |_| async {},
        )
        .await;
        assert_eq!(out, RecoveryOutcome::Abandoned);
    }

    #[tokio::test]
    async fn recovers_once_the_device_returns() {
        let opens = Cell::new(0u32);
        let installed = RefCell::new(Vec::new());
        let out = recover_stream(
            DeviceType::Microphone,
            policy(),
            || Some("dead"),
            |_| async {},
            || {
                opens.set(opens.get() + 1);
                let n = opens.get();
                async move { if n < 3 { Err(anyhow::anyhow!("not yet")) } else { Ok("fresh") } }
            },
            |s| { installed.borrow_mut().push(s); None },
            || true,
            |_| async {},
        )
        .await;
        assert_eq!(out, RecoveryOutcome::Recovered { attempts: 3 });
        assert_eq!(&*installed.borrow(), &["fresh"]);
    }

    #[tokio::test]
    async fn abandons_when_the_recording_ended() {
        let opens = Cell::new(0u32);
        let out = recover_stream(
            DeviceType::System,
            policy(),
            || None::<&str>,
            |_| async {},
            || { opens.set(opens.get() + 1); async { Ok("x") } },
            |s| Some(s),
            || false,
            |_| async {},
        )
        .await;
        assert_eq!(out, RecoveryOutcome::Abandoned);
        assert_eq!(opens.get(), 0, "no reopen against a finished recording");
    }

    #[tokio::test]
    async fn a_rejected_install_is_torn_down_not_leaked() {
        let torn = RefCell::new(Vec::new());
        let out = recover_stream(
            DeviceType::Microphone,
            policy(),
            || None::<&str>,
            |s| { torn.borrow_mut().push(s); async {} },
            || async { Ok("fresh") },
            |s| Some(s),
            || true,
            |_| async {},
        )
        .await;
        assert_eq!(out, RecoveryOutcome::Abandoned);
        assert_eq!(&*torn.borrow(), &["fresh"]);
    }

    /// Finding 2: a stalled cpal stop must not hold its caller (the mic swap)
    /// beyond the bound, and the swap flag must be free afterwards.
    #[tokio::test]
    async fn a_stalled_teardown_times_out_and_the_swap_flag_is_released() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = AtomicBool::new(false);
        {
            let guard = FlagGuard::try_acquire(&flag).expect("first acquire");
            assert!(FlagGuard::try_acquire(&flag).is_none(), "overlapping swap refused");
            let started = Instant::now();
            let timed_out = teardown_bounded(|| std::thread::sleep(Duration::from_secs(5)), Duration::from_millis(100)).await;
            assert!(timed_out);
            assert!(started.elapsed() < Duration::from_secs(2), "caller released at the bound, not after the stall");
            drop(guard);
        }
        assert!(!flag.load(Ordering::SeqCst), "flag cleared after the timeout path");
        assert!(FlagGuard::try_acquire(&flag).is_some(), "next swap can run");
    }

    #[test]
    fn the_flag_is_cleared_on_panic_unwind() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = AtomicBool::new(false);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = FlagGuard::try_acquire(&flag).unwrap();
            panic!("swap task panicked");
        }));
        assert!(r.is_err());
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_quick_teardown_reports_no_timeout() {
        assert!(!teardown_bounded(|| {}, Duration::from_secs(1)).await);
    }

    /// Finding 4 at the recovery level: when something else owns the slot by
    /// the time the attempts are over, the run is abandoned, never announced.
    #[tokio::test]
    async fn a_run_made_obsolete_mid_backoff_ends_silently() {
        let epoch = Cell::new(7u64);
        let captured = 7u64;
        let out = recover_stream(
            DeviceType::Microphone,
            policy(),
            || Some("dead"),
            |_| async {},
            || async { Err(anyhow::anyhow!("not back")) },
            |s: &str| Some(s),
            || epoch.get() == captured, // production: session live AND slot epoch unchanged
            |_| { epoch.set(8); async {} }, // the hot-swap lands during the first backoff
        )
        .await;
        assert_eq!(out, RecoveryOutcome::Abandoned);
    }

    #[test]
    fn ledger_stops_a_stream_that_keeps_dying_but_forgives_a_stable_one() {
        let mut l = RecoveryLedger::default();
        let t = Instant::now();
        for _ in 0..MAX_RAPID_RECOVERIES {
            assert!(l.allow(t));
            l.started();
            l.recovered(t);
        }
        assert!(!l.allow(t + Duration::from_secs(5)), "dies again right after recovery: stop");
        assert!(l.allow(t + STABLE_AFTER), "stayed healthy 30s: budget resets");
    }
}
