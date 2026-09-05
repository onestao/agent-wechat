use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::ia::identify_states;
use crate::sessions::manager::get_session;
use crate::tools::a11y::get_a11y_desktop;
use crate::tools::exec::ExecOptions;
use crate::tools::screenshot::capture_screenshot;
use crate::tools::wechat_db::find_wechat_pid;

/// How often to run the health scan (in seconds).
const SCAN_INTERVAL_SECS: u64 = 1;

/// Delay before restarting WeChat after a crash (in seconds).
const RESTART_DELAY_SECS: u64 = 3;

/// If WeChat crashes this many times within RAPID_WINDOW_SECS, back off.
const MAX_RAPID_RESTARTS: u32 = 5;
const RAPID_WINDOW_SECS: u64 = 60;
const BACKOFF_DELAY_SECS: u64 = 30;

/// Pure representation of health observation for decision testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthObservation {
    ProcessMissing,
    A11yUnavailable,
    Identified,
    Unidentified,
}

/// Pure representation of permitted health actions.
/// Note: Destructive kills of running processes are intentionally absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthAction {
    RestartMissingProcess,
    Healthy,
    ObserveDegraded,
}

/// Reason why WeChat observation entered degraded mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedReason {
    A11yUnavailable,
    Unidentified,
}

/// Internal health tracking state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedHealthState {
    Unknown,
    Healthy,
    Degraded(DegradedReason),
}

/// Pure logging decision produced by HealthLogTracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogDecision {
    /// First occurrence entering degraded state: emit WARN immediately.
    WarnTransition { reason: DegradedReason },
    /// Persistently degraded: emit rate-limited reminder WARN.
    WarnPeriodicReminder {
        reason: DegradedReason,
        elapsed_secs: u64,
        occurrences: u64,
    },
    /// Transitioned from Degraded back to Healthy: emit INFO recovery.
    InfoRecovered { previous_reason: DegradedReason },
    /// Suppress warning/info logs (healthy steady state or within throttle window).
    Suppress,
}

/// State-transition and rate-limiting tracker for degraded health logs.
#[derive(Debug, Clone)]
pub struct HealthLogTracker {
    state: TrackedHealthState,
    degraded_since: Option<Instant>,
    last_warn_at: Option<Instant>,
    degraded_count: u64,
    periodic_interval: Duration,
}

impl HealthLogTracker {
    pub fn new(periodic_interval: Duration) -> Self {
        Self {
            state: TrackedHealthState::Unknown,
            degraded_since: None,
            last_warn_at: None,
            degraded_count: 0,
            periodic_interval,
        }
    }

    pub fn reset(&mut self) {
        self.state = TrackedHealthState::Unknown;
        self.degraded_since = None;
        self.last_warn_at = None;
        self.degraded_count = 0;
    }

    pub fn step(&mut self, obs: HealthObservation, now: Instant) -> LogDecision {
        match obs {
            HealthObservation::A11yUnavailable | HealthObservation::Unidentified => {
                let reason = match obs {
                    HealthObservation::A11yUnavailable => DegradedReason::A11yUnavailable,
                    _ => DegradedReason::Unidentified,
                };
                self.degraded_count += 1;
                match self.state {
                    TrackedHealthState::Degraded(_) => {
                        self.state = TrackedHealthState::Degraded(reason);
                        let last = self.last_warn_at.unwrap_or(now);
                        if now.duration_since(last) >= self.periodic_interval {
                            self.last_warn_at = Some(now);
                            let elapsed_secs = self
                                .degraded_since
                                .map(|s| now.duration_since(s).as_secs())
                                .unwrap_or(0);
                            LogDecision::WarnPeriodicReminder {
                                reason,
                                elapsed_secs,
                                occurrences: self.degraded_count,
                            }
                        } else {
                            LogDecision::Suppress
                        }
                    }
                    _ => {
                        self.state = TrackedHealthState::Degraded(reason);
                        self.degraded_since = Some(now);
                        self.last_warn_at = Some(now);
                        LogDecision::WarnTransition { reason }
                    }
                }
            }
            HealthObservation::Identified => match self.state {
                TrackedHealthState::Degraded(prev_reason) => {
                    self.state = TrackedHealthState::Healthy;
                    self.degraded_since = None;
                    self.last_warn_at = None;
                    self.degraded_count = 0;
                    LogDecision::InfoRecovered {
                        previous_reason: prev_reason,
                    }
                }
                _ => {
                    self.state = TrackedHealthState::Healthy;
                    LogDecision::Suppress
                }
            },
            HealthObservation::ProcessMissing => {
                self.reset();
                LogDecision::Suppress
            }
        }
    }
}

impl Default for HealthLogTracker {
    fn default() -> Self {
        Self::new(Duration::from_secs(60))
    }
}

/// Decision function for health actions based on observation.
pub fn evaluate_health_action(obs: HealthObservation) -> HealthAction {
    match obs {
        HealthObservation::ProcessMissing => HealthAction::RestartMissingProcess,
        HealthObservation::Identified => HealthAction::Healthy,
        HealthObservation::A11yUnavailable | HealthObservation::Unidentified => {
            HealthAction::ObserveDegraded
        }
    }
}

/// Global flag to pause health monitoring during active execution loops.
static MONITORING_PAUSED: AtomicBool = AtomicBool::new(false);

/// Pause health monitoring (call when an execution loop starts).
pub fn pause_monitoring() {
    MONITORING_PAUSED.store(true, Ordering::Relaxed);
}

/// Resume health monitoring (call when an execution loop ends).
pub fn resume_monitoring() {
    MONITORING_PAUSED.store(false, Ordering::Relaxed);
}

/// Spawn WeChat process for the given session using the shared launch script.
fn spawn_wechat(session: &crate::ia::types::Session) {
    // Use DBUS_SESSION_BUS_ADDRESS from our own environment (inherited from
    // entrypoint.sh) rather than the DB value. The entrypoint's D-Bus session
    // is the one AT-SPI is connected to, so WeChat must use it for a11y to work.
    let result = std::process::Command::new("/opt/tools/launch-wechat")
        .env("DISPLAY", &session.display)
        .env("WECHAT_HOME", format!("/home/{}", session.linux_user))
        .env("WECHAT_USER", &session.linux_user)
        .spawn();

    match result {
        Ok(_) => tracing::info!("[health] Spawned WeChat for session '{}'", session.name),
        Err(e) => tracing::error!("[health] Failed to spawn WeChat: {}", e),
    }
}

/// Spawn the background health monitor task.
///
/// Every second, it checks the default session's WeChat process by running
/// a11y → identify. If WeChat has crashed, it restarts it.
/// If a11y fails or UI state is unidentified while the process is still running,
/// it logs a degraded observation and continues observing without killing.
pub fn spawn_health_monitor() {
    tokio::spawn(async move {
        tracing::info!("[health] WeChat health monitor started (non-destructive UI observation)");

        let mut was_running = false;
        let mut restart_count: u32 = 0;
        let mut window_start = Instant::now();
        let mut waiting_restart_since: Option<Instant> = None;
        let mut log_tracker = HealthLogTracker::default();

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(SCAN_INTERVAL_SECS)).await;

            // Skip if monitoring is paused (an execution loop is active)
            if MONITORING_PAUSED.load(Ordering::Relaxed) {
                continue;
            }

            // Only monitor the default session
            let session = match get_session("default") {
                Some(s) if s.status == "running" => s,
                _ => continue,
            };

            // Check if WeChat process is even running
            let wechat_pid = match find_wechat_pid() {
                Some(pid) => {
                    if !was_running {
                        tracing::info!("[health] WeChat process found (pid={})", pid);
                        was_running = true;
                        waiting_restart_since = None;
                    }
                    pid
                }
                None => {
                    let action = evaluate_health_action(HealthObservation::ProcessMissing);
                    debug_assert_eq!(action, HealthAction::RestartMissingProcess);

                    log_tracker.reset();
                    if was_running {
                        tracing::warn!(
                            "[health] WeChat process disappeared (likely crashed), restarting"
                        );
                        was_running = false;
                        waiting_restart_since = Some(Instant::now());
                    }

                    // Handle restart with crash loop protection
                    if let Some(since) = waiting_restart_since {
                        // Check crash loop
                        if window_start.elapsed().as_secs() > RAPID_WINDOW_SECS {
                            restart_count = 0;
                            window_start = Instant::now();
                        }

                        let delay = if restart_count >= MAX_RAPID_RESTARTS {
                            if since.elapsed().as_secs() == RESTART_DELAY_SECS {
                                tracing::warn!(
                                    "[health] Crash loop detected ({} restarts in {}s), backing off to {}s",
                                    restart_count, RAPID_WINDOW_SECS, BACKOFF_DELAY_SECS
                                );
                            }
                            BACKOFF_DELAY_SECS
                        } else {
                            RESTART_DELAY_SECS
                        };

                        if since.elapsed().as_secs() >= delay {
                            spawn_wechat(&session);
                            restart_count += 1;
                            waiting_restart_since = None;
                        }
                    }

                    continue;
                }
            };

            // Run a11y + identify to observe state
            let exec_options = ExecOptions {
                session: Some(session.clone()),
                timeout_ms: 10_000,
            };

            let a11y = match get_a11y_desktop(&exec_options).await {
                Ok(tree) => tree,
                Err(e) => {
                    let action = evaluate_health_action(HealthObservation::A11yUnavailable);
                    debug_assert_eq!(action, HealthAction::ObserveDegraded);
                    match log_tracker.step(HealthObservation::A11yUnavailable, Instant::now()) {
                        LogDecision::WarnTransition { .. } => {
                            tracing::warn!(
                                "[health] WeChat (pid={}) a11y query failed: {}; observation degraded, continuing without kill",
                                wechat_pid,
                                e
                            );
                        }
                        LogDecision::WarnPeriodicReminder {
                            elapsed_secs,
                            occurrences,
                            ..
                        } => {
                            tracing::warn!(
                                "[health] WeChat (pid={}) observation still degraded (a11y failed: {}) for {}s ({} checks); continuing without kill",
                                wechat_pid,
                                e,
                                elapsed_secs,
                                occurrences
                            );
                        }
                        LogDecision::InfoRecovered { .. } | LogDecision::Suppress => {}
                    }
                    continue;
                }
            };

            let screenshot = capture_screenshot(&exec_options).await.unwrap_or_default();
            let identified = identify_states(&a11y, &screenshot);

            if let Some(ref mw) = identified.main_window {
                let action = evaluate_health_action(HealthObservation::Identified);
                debug_assert_eq!(action, HealthAction::Healthy);
                if let LogDecision::InfoRecovered { previous_reason } =
                    log_tracker.step(HealthObservation::Identified, Instant::now())
                {
                    tracing::info!(
                        "[health] WeChat (pid={}) observation recovered to healthy from {:?}: identified as {:?}",
                        wechat_pid,
                        previous_reason,
                        mw.state_id
                    );
                }
                tracing::debug!(
                    "[health] WeChat (pid={}) alive, UI state identified: {:?}",
                    wechat_pid,
                    mw.state_id
                );
            } else {
                let action = evaluate_health_action(HealthObservation::Unidentified);
                debug_assert_eq!(action, HealthAction::ObserveDegraded);
                match log_tracker.step(HealthObservation::Unidentified, Instant::now()) {
                    LogDecision::WarnTransition { .. } => {
                        tracing::warn!(
                            "[health] WeChat (pid={}) alive, but UI state unidentified; observation degraded, continuing without kill",
                            wechat_pid
                        );
                    }
                    LogDecision::WarnPeriodicReminder {
                        elapsed_secs,
                        occurrences,
                        ..
                    } => {
                        tracing::warn!(
                            "[health] WeChat (pid={}) observation still degraded (UI unidentified) for {}s ({} checks); continuing without kill",
                            wechat_pid,
                            elapsed_secs,
                            occurrences
                        );
                    }
                    LogDecision::InfoRecovered { .. } | LogDecision::Suppress => {}
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_action_process_missing() {
        assert_eq!(
            evaluate_health_action(HealthObservation::ProcessMissing),
            HealthAction::RestartMissingProcess
        );
    }

    #[test]
    fn test_health_action_a11y_unavailable_never_kills() {
        let action = evaluate_health_action(HealthObservation::A11yUnavailable);
        assert_eq!(action, HealthAction::ObserveDegraded);
        assert_ne!(action, HealthAction::RestartMissingProcess);
    }

    #[test]
    fn test_health_action_unidentified_ui_never_kills() {
        let action = evaluate_health_action(HealthObservation::Unidentified);
        assert_eq!(action, HealthAction::ObserveDegraded);
        assert_ne!(action, HealthAction::RestartMissingProcess);
    }

    #[test]
    fn test_health_action_identified() {
        assert_eq!(
            evaluate_health_action(HealthObservation::Identified),
            HealthAction::Healthy
        );
    }

    #[test]
    fn test_degraded_first_occurrence_logs_warn() {
        let mut tracker = HealthLogTracker::default();
        let now = Instant::now();

        let decision = tracker.step(HealthObservation::A11yUnavailable, now);
        assert_eq!(
            decision,
            LogDecision::WarnTransition {
                reason: DegradedReason::A11yUnavailable
            }
        );
    }

    #[test]
    fn test_repeated_degraded_observations_rate_limited() {
        let mut tracker = HealthLogTracker::new(Duration::from_secs(60));
        let start = Instant::now();

        // First observation produces WarnTransition
        let d0 = tracker.step(HealthObservation::Unidentified, start);
        assert!(matches!(d0, LogDecision::WarnTransition { .. }));

        // Rapid subsequent checks within 60s are suppressed
        for sec in 1..60 {
            let d = tracker.step(
                HealthObservation::Unidentified,
                start + Duration::from_secs(sec),
            );
            assert_eq!(
                d,
                LogDecision::Suppress,
                "Second {sec} should be suppressed"
            );
        }

        // At exactly 60s, emit periodic reminder
        let d60 = tracker.step(
            HealthObservation::Unidentified,
            start + Duration::from_secs(60),
        );
        assert_eq!(
            d60,
            LogDecision::WarnPeriodicReminder {
                reason: DegradedReason::Unidentified,
                elapsed_secs: 60,
                occurrences: 61,
            }
        );
    }

    #[test]
    fn test_degraded_to_healthy_recovery_logs_info() {
        let mut tracker = HealthLogTracker::default();
        let start = Instant::now();

        let _ = tracker.step(HealthObservation::A11yUnavailable, start);
        let recovery = tracker.step(
            HealthObservation::Identified,
            start + Duration::from_secs(5),
        );
        assert_eq!(
            recovery,
            LogDecision::InfoRecovered {
                previous_reason: DegradedReason::A11yUnavailable
            }
        );

        // Further healthy steps remain suppressed (debug logging only)
        let healthy_steady = tracker.step(
            HealthObservation::Identified,
            start + Duration::from_secs(6),
        );
        assert_eq!(healthy_steady, LogDecision::Suppress);
    }

    #[test]
    fn test_degraded_ten_minutes_bounded_warnings() {
        let mut tracker = HealthLogTracker::new(Duration::from_secs(60));
        let start = Instant::now();
        let mut warn_count = 0;

        // 10 minutes = 600 seconds with 1 check per second
        for sec in 0..=600 {
            let d = tracker.step(
                HealthObservation::Unidentified,
                start + Duration::from_secs(sec),
            );
            match d {
                LogDecision::WarnTransition { .. } | LogDecision::WarnPeriodicReminder { .. } => {
                    warn_count += 1;
                }
                _ => {}
            }
        }

        // In 600 seconds at 60s interval: exactly 11 warnings (t=0, 60, 120, ..., 600)
        assert_eq!(warn_count, 11);
        assert!(
            warn_count <= 600,
            "10-minute warnings must be bounded <= 600"
        );
    }

    #[test]
    fn test_alternating_degraded_reasons_stay_rate_limited() {
        let mut tracker = HealthLogTracker::new(Duration::from_secs(60));
        let start = Instant::now();

        // t=0: A11yUnavailable
        let d0 = tracker.step(HealthObservation::A11yUnavailable, start);
        assert!(matches!(d0, LogDecision::WarnTransition { .. }));

        // t=1: Unidentified (flicker between degraded modes)
        let d1 = tracker.step(
            HealthObservation::Unidentified,
            start + Duration::from_secs(1),
        );
        // Must remain suppressed, not trigger a new WarnTransition every second
        assert_eq!(d1, LogDecision::Suppress);
    }

    #[test]
    fn test_process_missing_resets_tracker() {
        let mut tracker = HealthLogTracker::default();
        let start = Instant::now();

        let _ = tracker.step(HealthObservation::A11yUnavailable, start);
        let d_missing = tracker.step(
            HealthObservation::ProcessMissing,
            start + Duration::from_secs(2),
        );
        assert_eq!(d_missing, LogDecision::Suppress);

        // After process missing resets tracker, a subsequent degraded observation logs a fresh transition
        let d_fresh = tracker.step(
            HealthObservation::Unidentified,
            start + Duration::from_secs(10),
        );
        assert_eq!(
            d_fresh,
            LogDecision::WarnTransition {
                reason: DegradedReason::Unidentified
            }
        );
    }
}
