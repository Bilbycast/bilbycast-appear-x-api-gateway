// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Client-side event rate limiter (Phase 5 scale-out alignment).
//!
//! The bilbycast-manager enforces a per-node event rate limit of
//! 1000 events/minute — anything over is dropped silently on the
//! manager side and a single `event_rate_limit_exceeded` warning
//! event is synthesised per window. Losing information on the
//! wire is worse than coalescing at the source, so this gate
//! sits in front of the gateway's event emission path and:
//!
//! * allows events through until the sliding 60 s window is 95 %
//!   full (950 events),
//! * drops further events in the window but counts the drops,
//! * on window roll (or when the first event after a suppressed
//!   run fires), emits ONE summary `event_rate_limit_selfgate`
//!   event announcing the suppressed count.
//!
//! The 95 % cap sits below the manager's 100 % so a brief burst
//! never races past the gate AND the manager rate limit in the
//! same 60 s window — client-side self-suppression always wins,
//! which means the operator sees the exact count of dropped
//! events instead of a silent manager-side clamp.
//!
//! The SDK does not (yet) expose a rate-limit helper, so the
//! Phase 6 migration keeps this gate in the gateway. A future
//! SDK release is expected to promote it once a second vendor
//! gateway exists.

use std::sync::Mutex;

use bilbycast_gateway_sdk::{EventSeverity, GatewayEvent};
use chrono::{DateTime, Duration, Utc};
use serde_json::json;

/// Hard cap (events / minute) that stays strictly below the
/// manager's 1000/min limit so self-gating always trips first.
const SELF_GATE_LIMIT_PER_MINUTE: u32 = 950;
const WINDOW_SECS: i64 = 60;

#[derive(Debug)]
pub struct EventGate {
    state: Mutex<WindowState>,
}

#[derive(Debug, Clone)]
struct WindowState {
    window_start: DateTime<Utc>,
    accepted: u32,
    suppressed: u32,
}

/// What the caller should do with the event being proposed.
#[derive(Debug)]
pub enum GateDecision {
    /// Send the event as-is.
    Send,
    /// Suppress the event. Caller can still emit the optional
    /// summary when provided.
    Suppress {
        /// Summary event to emit in place of the dropped event so
        /// the operator sees that suppression is happening.
        /// `None` if the gate hasn't yet crossed its cap in this
        /// window.
        summary: Option<GatewayEvent>,
    },
    /// A prior window rolled over and had suppressed events —
    /// emit this rollover summary AND then send the proposed
    /// event. Covers the narrow race where window N ends on a
    /// suppressed tail and window N+1 opens with an accepted
    /// event.
    SendWithRollover {
        summary: GatewayEvent,
    },
}

impl EventGate {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(WindowState {
                window_start: Utc::now(),
                accepted: 0,
                suppressed: 0,
            }),
        }
    }

    /// Register a proposed event emission. Caller uses the
    /// returned [`GateDecision`] to either forward the event or
    /// drop it. Thread-safe; the lock is held for µs.
    pub fn check(&self) -> GateDecision {
        let now = Utc::now();
        let mut st = self.state.lock().expect("gate mutex poisoned");
        // Window roll: if we crossed a 60 s boundary since the
        // last check, emit a summary for the PRIOR window's
        // suppressed count before resetting.
        let mut summary_on_roll: Option<GatewayEvent> = None;
        if now.signed_duration_since(st.window_start) >= Duration::seconds(WINDOW_SECS) {
            if st.suppressed > 0 {
                summary_on_roll = Some(selfgate_summary(st.suppressed, st.accepted));
            }
            st.window_start = now;
            st.accepted = 0;
            st.suppressed = 0;
        }

        if st.accepted < SELF_GATE_LIMIT_PER_MINUTE {
            st.accepted = st.accepted.saturating_add(1);
            if let Some(summary) = summary_on_roll {
                return GateDecision::SendWithRollover { summary };
            }
            return GateDecision::Send;
        }

        st.suppressed = st.suppressed.saturating_add(1);
        // We emit the summary event ONCE per window — on the
        // first suppressed event after crossing the cap.
        let summary = if st.suppressed == 1 {
            Some(selfgate_summary(st.suppressed, st.accepted))
        } else {
            None
        };
        GateDecision::Suppress { summary }
    }
}

impl Default for EventGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the `event_rate_limit_selfgate` summary event. Severity
/// is `minor` — the SDK's `EventSeverity` enum uses the edge/relay
/// taxonomy (info / minor / major / critical).
fn selfgate_summary(suppressed: u32, accepted: u32) -> GatewayEvent {
    GatewayEvent::new(
        EventSeverity::Minor,
        "event_rate_limit_selfgate",
        format!(
            "Gateway self-rate-limiter engaged: {suppressed} event(s) suppressed \
             (accepted {accepted} in the current 60s window). \
             Raise the polling cadence only if the manager can absorb more."
        ),
    )
    .with_details(json!({
        "suppressed": suppressed,
        "accepted": accepted,
        "limit_per_minute": SELF_GATE_LIMIT_PER_MINUTE,
        "manager_limit_per_minute": 1000,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_event_passes_without_summary() {
        let gate = EventGate::new();
        assert!(matches!(gate.check(), GateDecision::Send));
    }

    #[test]
    fn over_cap_suppresses_with_single_summary() {
        let gate = EventGate::new();
        for _ in 0..SELF_GATE_LIMIT_PER_MINUTE {
            assert!(matches!(gate.check(), GateDecision::Send));
        }
        // First over-cap event yields a summary.
        match gate.check() {
            GateDecision::Suppress { summary: Some(_) } => {}
            other => panic!("expected Suppress with summary; got {other:?}"),
        }
        // Subsequent over-cap events suppress silently.
        match gate.check() {
            GateDecision::Suppress { summary: None } => {}
            other => panic!("expected silent Suppress; got {other:?}"),
        }
    }
}
