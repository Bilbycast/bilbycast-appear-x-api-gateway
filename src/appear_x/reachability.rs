// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Per-target reachability state for the Appear X gateway.
//!
//! Tracks consecutive poll failures, the last successful poll timestamp,
//! and the most recent error *code* (a fixed enum, never the verbose vendor
//! error string). Drives:
//! - The `gateway_target.reachable` flag emitted on every health heartbeat
//!   (manager → dashboard third-state amber rendering).
//! - Edge-triggered, dwell-gated `target_unreachable` / `target_recovered`
//!   events emitted on state flips that have been stable past the
//!   configured dwell window — defeats slow-flap noise.
//!
//! Update path runs once per alarm poll (default 10 s cadence) so the
//! `Mutex<Option<String>>` for `last_error_code` is fine; everything else
//! is a plain atomic.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

use chrono::Utc;

/// Outcome of `record_success` / `record_failure`. Drives whether the
/// caller should fire a `target_unreachable` / `target_recovered` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// State did not change (or the new state hasn't dwelled long enough).
    NoChange,
    /// `reachable` flipped to `false` and the new state has been stable
    /// past `event_dwell_secs` — caller should emit `target_unreachable`.
    BecameUnreachable {
        consecutive_failures: u32,
        last_error_code: Option<String>,
    },
    /// `reachable` flipped to `true` after a previous unreachable streak
    /// stable past `event_dwell_secs` — caller should emit
    /// `target_recovered` with `downtime_secs`.
    Recovered {
        downtime_secs: i64,
    },
}

/// Reachability tracker for one polled target.
#[derive(Debug)]
pub struct ReachabilityState {
    /// Number of consecutive failed polls since the last success.
    /// `0` means the most recent poll succeeded.
    consecutive_failures: AtomicU32,
    /// Threshold at which `is_reachable()` flips to `false`. Configurable
    /// via the gateway's `[appear_x] reachability_failure_threshold`.
    failure_threshold: u32,
    /// Unix-seconds timestamp of the last successful poll, or 0 if none yet.
    last_success_unix: AtomicI64,
    /// Last reported `reachable` state — used to detect edge transitions.
    /// Initialised to `true` so the first poll can decide.
    last_reachable_state: AtomicBool,
    /// Unix-seconds timestamp of when the most recent unreachable streak
    /// began, or 0 if currently reachable.
    unreachable_since_unix: AtomicI64,
    /// Unix-seconds timestamp of the most recent `reachable` flip (used
    /// for dwell-time gating of `target_*` events).
    last_state_flip_unix: AtomicI64,
    /// Whether we've already fired the `target_unreachable` /
    /// `target_recovered` event for the current state — single-fire per
    /// flip, gated behind the dwell window.
    fired_event_for_current_state: AtomicBool,
    /// Dwell time (seconds) the new state must be stable before the
    /// caller fires a `target_*` event.
    event_dwell_secs: i64,
    /// Most recent error code from a failed poll (fixed enum, NOT the
    /// verbose vendor error string).
    last_error_code: Mutex<Option<String>>,
}

impl ReachabilityState {
    pub fn new(failure_threshold: u32, event_dwell_secs: u64) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            failure_threshold: failure_threshold.max(1),
            last_success_unix: AtomicI64::new(0),
            last_reachable_state: AtomicBool::new(true),
            unreachable_since_unix: AtomicI64::new(0),
            last_state_flip_unix: AtomicI64::new(0),
            fired_event_for_current_state: AtomicBool::new(true),
            event_dwell_secs: event_dwell_secs as i64,
            last_error_code: Mutex::new(None),
        }
    }

    /// Mark the most recent poll as successful. Returns whether the caller
    /// should fire a `target_recovered` event (only on the first success
    /// after a sustained unreachable streak that has dwelled long enough).
    pub fn record_success(&self) -> TransitionOutcome {
        let now = Utc::now().timestamp();
        self.consecutive_failures.store(0, Ordering::Release);
        self.last_success_unix.store(now, Ordering::Release);
        if let Ok(mut guard) = self.last_error_code.lock() {
            *guard = None;
        }
        let was_reachable = self.last_reachable_state.swap(true, Ordering::AcqRel);
        if was_reachable {
            return TransitionOutcome::NoChange;
        }
        // Edge: false → true. Compute downtime, update flip timestamp,
        // reset the per-state single-fire latch, and fire (no dwell on
        // recovery — good news doesn't need suppression).
        let unreachable_since = self.unreachable_since_unix.swap(0, Ordering::AcqRel);
        let downtime_secs = if unreachable_since > 0 { now - unreachable_since } else { 0 };
        self.last_state_flip_unix.store(now, Ordering::Release);
        self.fired_event_for_current_state.store(false, Ordering::Release);
        self.maybe_fire_recovered(downtime_secs)
    }

    /// Mark the most recent poll as failed with a classified error code.
    /// Returns whether the caller should fire a `target_unreachable`
    /// event (only on the first sustained transition past the dwell
    /// window — single-fire per flip).
    pub fn record_failure(&self, error_code: &str) -> TransitionOutcome {
        let now = Utc::now().timestamp();
        let new_count = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut guard) = self.last_error_code.lock() {
            *guard = Some(error_code.to_string());
        }
        if new_count < self.failure_threshold {
            // Not yet over the threshold — caller still considers reachable.
            return TransitionOutcome::NoChange;
        }
        let was_reachable = self.last_reachable_state.swap(false, Ordering::AcqRel);
        if was_reachable {
            // Edge: true → false. Stamp transition, but don't fire until dwell.
            self.unreachable_since_unix.store(now, Ordering::Release);
            self.last_state_flip_unix.store(now, Ordering::Release);
            self.fired_event_for_current_state.store(false, Ordering::Release);
        }
        self.maybe_fire_unreachable(new_count)
    }

    /// Whether the gateway should currently report `reachable: true` to
    /// the manager. Reflects the threshold state, not the per-poll outcome.
    pub fn is_reachable(&self) -> bool {
        self.last_reachable_state.load(Ordering::Acquire)
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Acquire)
    }

    pub fn last_success_unix(&self) -> Option<i64> {
        let v = self.last_success_unix.load(Ordering::Acquire);
        if v == 0 { None } else { Some(v) }
    }

    pub fn last_error_code(&self) -> Option<String> {
        self.last_error_code.lock().ok().and_then(|g| g.clone())
    }

    fn maybe_fire_unreachable(&self, consecutive_failures: u32) -> TransitionOutcome {
        let now = Utc::now().timestamp();
        let flipped_at = self.last_state_flip_unix.load(Ordering::Acquire);
        if flipped_at == 0 || now - flipped_at < self.event_dwell_secs {
            return TransitionOutcome::NoChange;
        }
        // Single-fire per flip: only return BecameUnreachable on the first
        // post-dwell tick after the transition.
        let already = self.fired_event_for_current_state.swap(true, Ordering::AcqRel);
        if already {
            return TransitionOutcome::NoChange;
        }
        TransitionOutcome::BecameUnreachable {
            consecutive_failures,
            last_error_code: self.last_error_code(),
        }
    }

    fn maybe_fire_recovered(&self, downtime_secs: i64) -> TransitionOutcome {
        // For recovery we fire on the very next success after an
        // unreachable streak — no extra dwell needed (recovery is good
        // news, no need to suppress). But honour single-fire so a flapping
        // line doesn't spam recoveries.
        let already = self.fired_event_for_current_state.swap(true, Ordering::AcqRel);
        if already {
            return TransitionOutcome::NoChange;
        }
        TransitionOutcome::Recovered { downtime_secs }
    }
}

/// Classify an error from the JSON-RPC client into the fixed enum reported
/// in `gateway_target.last_error_code`. Verbose strings stay in the
/// gateway's local event log — never on the wire.
pub fn classify_jsonrpc_error(err: &anyhow::Error) -> &'static str {
    let msg = format!("{err:#}").to_lowercase();
    if msg.contains("timed out") || msg.contains("timeout") {
        "http_timeout"
    } else if msg.contains("connection refused") || msg.contains("refused") {
        "tcp_refused"
    } else if msg.contains("tls") || msg.contains("certificate") || msg.contains("handshake") {
        "tls_handshake"
    } else if msg.contains("401") || msg.contains("403") || msg.contains("auth") || msg.contains("login") {
        "auth_rejected"
    } else if msg.contains("json") || msg.contains("rpc") || msg.contains("parse") {
        "rpc_protocol_error"
    } else {
        "other"
    }
}

/// Best-effort egress-IP probe: open a UDP socket and look at the local
/// address the kernel selected for routing toward `8.8.8.8:80`. No packets
/// are sent. Returns `None` on any I/O error (e.g. no default route).
pub fn detect_egress_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip().to_string())
}

/// Best-effort hostname lookup. Tries `/proc/sys/kernel/hostname`
/// (Linux), then `/etc/hostname`, then the `HOSTNAME` env var. Returns
/// `None` if all paths fail.
pub fn detect_hostname() -> Option<String> {
    let trim = |s: String| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    };
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        if let Some(t) = trim(s) { return Some(t); }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        if let Some(t) = trim(s) { return Some(t); }
    }
    std::env::var("HOSTNAME").ok().and_then(trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_event_below_threshold() {
        let r = ReachabilityState::new(2, 0);
        assert_eq!(r.record_failure("http_timeout"), TransitionOutcome::NoChange);
        assert!(r.is_reachable());
        assert_eq!(r.consecutive_failures(), 1);
    }

    #[test]
    fn fires_unreachable_after_threshold_with_zero_dwell() {
        let r = ReachabilityState::new(2, 0);
        assert_eq!(r.record_failure("http_timeout"), TransitionOutcome::NoChange);
        match r.record_failure("http_timeout") {
            TransitionOutcome::BecameUnreachable { consecutive_failures, last_error_code } => {
                assert_eq!(consecutive_failures, 2);
                assert_eq!(last_error_code.as_deref(), Some("http_timeout"));
            }
            other => panic!("expected BecameUnreachable, got {other:?}"),
        }
        assert!(!r.is_reachable());
    }

    #[test]
    fn single_fire_per_flip() {
        let r = ReachabilityState::new(1, 0);
        assert!(matches!(
            r.record_failure("tcp_refused"),
            TransitionOutcome::BecameUnreachable { .. }
        ));
        // Subsequent failures while still unreachable do not re-fire.
        assert_eq!(r.record_failure("tcp_refused"), TransitionOutcome::NoChange);
        assert_eq!(r.record_failure("tcp_refused"), TransitionOutcome::NoChange);
    }

    #[test]
    fn fires_recovered_on_first_success_after_unreachable() {
        let r = ReachabilityState::new(1, 0);
        let _ = r.record_failure("http_timeout");
        match r.record_success() {
            TransitionOutcome::Recovered { .. } => {}
            other => panic!("expected Recovered, got {other:?}"),
        }
        // Subsequent successes do not re-fire.
        assert_eq!(r.record_success(), TransitionOutcome::NoChange);
    }

    #[test]
    fn dwell_suppresses_unreachable_until_satisfied() {
        // 1-second dwell — even with threshold=1, the first failure should
        // NOT fire because zero seconds have elapsed since the flip.
        let r = ReachabilityState::new(1, 1);
        assert_eq!(r.record_failure("other"), TransitionOutcome::NoChange);
        assert!(!r.is_reachable());
    }

    #[test]
    fn classify_codes_cover_common_cases() {
        assert_eq!(classify_jsonrpc_error(&anyhow::anyhow!("operation timed out")), "http_timeout");
        assert_eq!(classify_jsonrpc_error(&anyhow::anyhow!("connection refused")), "tcp_refused");
        assert_eq!(classify_jsonrpc_error(&anyhow::anyhow!("tls handshake failed: bad certificate")), "tls_handshake");
        assert_eq!(classify_jsonrpc_error(&anyhow::anyhow!("HTTP 401 Unauthorized")), "auth_rejected");
        assert_eq!(classify_jsonrpc_error(&anyhow::anyhow!("invalid JSON-RPC response")), "rpc_protocol_error");
        assert_eq!(classify_jsonrpc_error(&anyhow::anyhow!("something weird")), "other");
    }
}
