// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Polling engine that periodically fetches data from the Appear X unit
//! and writes it into [`SharedAppearXState`]. A single emitter task
//! periodically snapshots that state and sends one consolidated `stats`
//! message to the manager via the SDK's [`Emitter`].
//!
//! This avoids the "last payload wins" problem on the manager side, where
//! every separate `stats` message would overwrite the previous one in
//! `cached_stats`. The manager-side dashboard always sees a complete picture
//! (chassis + slots + inputs + outputs + ip_interfaces + alarms) in a single
//! payload.

use anyhow::Result;
use bilbycast_gateway_sdk::{Emitter, EventSeverity, GatewayEvent};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::capabilities::DeviceCapabilities;
use super::jsonrpc::JsonRpcClient;
use super::state::SharedAppearXState;
use crate::config::PollingConfig;
use crate::event_gate::{EventGate, GateDecision};

/// Interval (seconds) at which the consolidated stats snapshot is pushed to
/// the manager. Independent of the per-source polling cadences.
const STATS_EMIT_INTERVAL_SECS: u64 = 5;

/// Run the polling engine — spawns tasks for each poll type plus a single
/// stats emitter that ships a consolidated snapshot to the manager.
///
/// `caps` is the result of [`crate::appear_x::capabilities::discover`] and
/// drives which per-slot tasks are spawned. Per-slot polling is only set up
/// for slots whose discovery actually found a matching card-level interface
/// (e.g. `ipGateway`); slots whose card software uses an unknown namespace
/// silently skip per-slot polling rather than spamming "Method not found".
pub async fn run_polling(
    client: JsonRpcClient,
    config: PollingConfig,
    caps: DeviceCapabilities,
    state: SharedAppearXState,
    emitter: Emitter,
    cancel: CancellationToken,
) -> Result<()> {
    // Authenticate once at startup
    client.authenticate().await?;

    // Client-side event rate gate matching the manager's per-node
    // 1000/min limit. Stays below the manager cap at 950/min so
    // self-gating trips first and the operator sees the exact
    // drop count in a summary event. Shared across every polling
    // task since they all share the outbound event quota.
    //
    // The SDK does not expose a rate-limiter helper as of v0.1, so
    // this lives locally. See `event_gate.rs` for the full rationale.
    let event_gate = Arc::new(EventGate::new());

    // Spawn alarm polling (MMI endpoint).
    //
    // The alarm poller is special: in addition to writing into the shared
    // state, it ALSO sends an immediate `health` envelope to the manager so
    // alarm-driven health flips ("ok" → "critical") are visible without
    // waiting for the next stats emit tick.
    let alarms_method = format!("mmi:{}/alarms/GetActiveAlarms", config.alarms_mmi_version);
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        config.alarms_interval_secs,
        "alarms",
        {
            let emitter = emitter.clone();
            let event_gate = event_gate.clone();
            move |c, st| {
                let alarms_method = alarms_method.clone();
                let emitter = emitter.clone();
                let event_gate = event_gate.clone();
                Box::pin(async move {
                    let result = c.call_mmi(&alarms_method, json!({"query": {}})).await?;
                    let alarms_value = result.get("data").cloned().unwrap_or(json!([]));
                    let alarm_list: Vec<Value> =
                        alarms_value.as_array().cloned().unwrap_or_default();

                    let status = derive_status(&alarm_list);
                    let (new_alarms, cleared_ids) =
                        st.set_alarms(alarm_list, status).await;

                    // Emit manager events for newly raised alarms.
                    for alarm in &new_alarms {
                        let severity = alarm
                            .get("severity")
                            .and_then(|s| s.as_str())
                            .unwrap_or("UNKNOWN");
                        let alarm_id = alarm
                            .get("alarmId")
                            .and_then(|s| s.as_str())
                            .unwrap_or("unknown");
                        let alarm_name = alarm
                            .get("alarmName")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let description = alarm
                            .get("alarmDescription")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let slot = alarm
                            .get("configObjectSlot")
                            .and_then(|s| s.as_u64());
                        let object_label = alarm
                            .get("configObjectLabel")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        let event_severity = match severity {
                            "CRITICAL" | "MAJOR" => EventSeverity::Critical,
                            "MINOR" | "WARNING" => EventSeverity::Minor,
                            _ => EventSeverity::Info,
                        };
                        let message = if alarm_name.is_empty() {
                            format!("Alarm raised: {description}")
                        } else {
                            format!("Alarm raised: {alarm_name} — {description}")
                        };
                        let mut details = json!({
                            "alarm_id": alarm_id,
                            "severity": severity,
                        });
                        if let Some(s) = slot {
                            details["slot"] = json!(s);
                        }
                        if !object_label.is_empty() {
                            details["object"] = json!(object_label);
                        }
                        let event = GatewayEvent::new(event_severity, "alarm", message)
                            .with_details(details);
                        emit_gated_event(&emitter, &event_gate, event).await;
                    }

                    // Emit info events for cleared alarms.
                    for alarm_id in &cleared_ids {
                        let event = GatewayEvent::info(
                            "alarm",
                            format!("Alarm cleared: {alarm_id}"),
                        )
                        .with_details(json!({ "alarm_id": alarm_id }));
                        emit_gated_event(&emitter, &event_gate, event).await;
                    }

                    // Push a fast `health` notification so the dashboard pill
                    // updates between stats emit ticks.
                    let health = json!({
                        "status": status,
                        "alarms": alarms_value,
                        "version": env!("CARGO_PKG_VERSION"),
                    });
                    let _ = emitter.emit_health(health).await;
                    Ok(())
                })
            }
        },
    );

    // Spawn chassis polling (MMI endpoint)
    let chassis_method = format!("mmi:{}/chassisModel/GetGraph", config.chassis_mmi_version);
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        config.chassis_interval_secs,
        "chassis",
        move |c, st| {
            let chassis_method = chassis_method.clone();
            Box::pin(async move {
                let result = c.call_mmi(&chassis_method, json!({})).await?;
                st.set_chassis(result).await;
                Ok(())
            })
        },
    );

    // Spawn cards polling (chassis-info + per-slot card software state).
    // This is the canonical source of per-slot info on X5 / X20 firmware
    // (`cards/GetChassisInfo` and `cards/GetCardStates`) and is independent of
    // the per-board ipGateway polling, which only applies to certain card types.
    let cards_info_method = format!("mmi:{}/cards/GetChassisInfo", config.cards_mmi_version);
    let cards_state_method = format!("mmi:{}/cards/GetCardStates", config.cards_mmi_version);
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        config.cards_interval_secs,
        "cards",
        move |c, st| {
            let cards_info_method = cards_info_method.clone();
            let cards_state_method = cards_state_method.clone();
            Box::pin(async move {
                let info = c.call_mmi(&cards_info_method, json!({})).await?;
                let states = c.call_mmi(&cards_state_method, json!({})).await?;
                st.set_cards(
                    info.get("data").cloned().unwrap_or(json!({})),
                    states.get("cards").cloned().unwrap_or(json!([])),
                )
                .await;
                Ok(())
            })
        },
    );

    // Spawn per-slot polling, gated on what discovery actually found.
    //
    // For each slot we walk the discovered interfaces and only spawn pollers
    // backed by interface families this firmware confirmed it speaks. Slots on
    // card softwares whose namespace isn't yet in `probe_registry::CARD_PROBES`
    // (e.g. the X5 HEVC SDI demo unit at the time of writing) silently
    // contribute zero per-slot pollers, while still being reported through the
    // chassis-level alarms / chassisModel / cards polls above.
    info!(
        "Setting up per-slot polling for {} slot(s) on {} chassis",
        caps.slots.len(),
        caps.chassis_type
    );
    for (slot, slot_caps) in &caps.slots {
        let slot = *slot;
        if slot_caps.discovered_interfaces.is_empty() {
            warn!(
                "Slot {} ({}, software_id={:?}): no card-level interfaces matched the \
                 probe registry — only chassis-level polls will report data for this slot. \
                 To add support, register the firmware's namespace in \
                 src/appear_x/probe_registry.rs.",
                slot, slot_caps.name, slot_caps.software_id
            );
            continue;
        }

        // Legacy IP Gateway boards (ME-3000 / ME-4000 family).
        if let Some(rec) = slot_caps.discovered_interfaces.get("ipGateway") {
            let version = rec.version.clone();
            info!(
                "Slot {}: spawning ipGateway pollers (version {})",
                slot, version
            );

            // Input polling
            let v = version.clone();
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                config.inputs_interval_secs,
                &format!("inputs-slot{}", slot),
                move |c, st| {
                    let v = v.clone();
                    Box::pin(async move {
                        let method = format!("ipGateway:{}/input/GetInputs", v);
                        let result = c.call_board(slot, &method, json!({})).await?;
                        st.set_slot_inputs(
                            slot,
                            result.get("data").cloned().unwrap_or(json!([])),
                        )
                        .await;
                        Ok(())
                    })
                },
            );

            // Output polling
            let v = version.clone();
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                config.outputs_interval_secs,
                &format!("outputs-slot{}", slot),
                move |c, st| {
                    let v = v.clone();
                    Box::pin(async move {
                        let method = format!("ipGateway:{}/output/GetOutputs", v);
                        let result = c.call_board(slot, &method, json!({})).await?;
                        st.set_slot_outputs(
                            slot,
                            result.get("data").cloned().unwrap_or(json!([])),
                        )
                        .await;
                        Ok(())
                    })
                },
            );

            // IP interfaces polling
            let v = version.clone();
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                config.inputs_interval_secs * 2, // less frequent
                &format!("interfaces-slot{}", slot),
                move |c, st| {
                    let v = v.clone();
                    Box::pin(async move {
                        let method = format!("ipGateway:{}/ipinterface/GetIpInterfaces", v);
                        let result = c.call_board(slot, &method, json!({})).await?;
                        st.set_slot_ip_interfaces(
                            slot,
                            result.get("data").cloned().unwrap_or(json!([])),
                        )
                        .await;
                        Ok(())
                    })
                },
            );
        }

        // Note: per-card-family polling for the Xger (IP 2110), hipEnc (JPEG XS),
        // and `sdi` (SDI JPEG XS) families is registered for discovery but not
        // yet wired up to dedicated pollers here. Once a real test unit of one
        // of those families is available, add a branch above analogous to the
        // `ipGateway` block.
    }

    // Single consolidated stats emitter — snapshots SharedAppearXState every
    // STATS_EMIT_INTERVAL_SECS and ships ONE merged payload to the manager.
    {
        let emitter = emitter.clone();
        let state = state.clone();
        let emit_cancel = cancel.child_token();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(STATS_EMIT_INTERVAL_SECS));
            interval.tick().await; // skip the immediate tick
            loop {
                tokio::select! {
                    _ = emit_cancel.cancelled() => break,
                    _ = interval.tick() => {
                        let snapshot = state.snapshot().await;
                        if emitter.emit_stats(snapshot).await.is_err() {
                            debug!("stats_emitter: channel closed, exiting");
                            break;
                        }
                    }
                }
            }
        });
    }

    // Wait for cancellation
    cancel.cancelled().await;
    Ok(())
}

/// Route an event through the client-side [`EventGate`] before
/// handing it to the SDK emitter. On suppression, we still forward
/// the optional summary the gate produced so the operator sees
/// exactly how many events were dropped in the window.
async fn emit_gated_event(emitter: &Emitter, gate: &Arc<EventGate>, event: GatewayEvent) {
    match gate.check() {
        GateDecision::Send => {
            let _ = emitter.emit_event(event).await;
        }
        GateDecision::SendWithRollover { summary } => {
            // Rollover summary for the prior window first, then the
            // current event — ordering preserves what the operator
            // sees in the event log.
            let _ = emitter.emit_event(summary).await;
            let _ = emitter.emit_event(event).await;
        }
        GateDecision::Suppress { summary } => {
            // Drop the original — manager's own 1000/min limit
            // would catch it anyway and drop silently, so
            // dropping here loses no information.
            if let Some(s) = summary {
                let _ = emitter.emit_event(s).await;
            }
        }
    }
}

fn derive_status(alarms: &[Value]) -> &'static str {
    let has_major = alarms.iter().any(|alarm| {
        alarm
            .get("severity")
            .and_then(|s| s.as_str())
            .map(|s| s == "MAJOR" || s == "CRITICAL")
            .unwrap_or(false)
    });
    if has_major {
        return "critical";
    }
    let has_minor = alarms.iter().any(|alarm| {
        alarm
            .get("severity")
            .and_then(|s| s.as_str())
            .map(|s| s == "MINOR" || s == "WARNING")
            .unwrap_or(false)
    });
    if has_minor {
        "degraded"
    } else {
        "ok"
    }
}

fn spawn_state_poll<F>(
    client: JsonRpcClient,
    state: SharedAppearXState,
    cancel: CancellationToken,
    interval_secs: u64,
    name: &str,
    poll_fn: F,
) where
    F: Fn(
            JsonRpcClient,
            SharedAppearXState,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync
        + 'static,
{
    let name = name.to_string();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.tick().await; // skip first immediate tick

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    match poll_fn(client.clone(), state.clone()).await {
                        Ok(()) => {
                            debug!("Poll {} succeeded", name);
                        }
                        Err(e) => {
                            error!("Poll {} failed: {}", name, e);
                        }
                    }
                }
            }
        }
    });
}
