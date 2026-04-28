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
//! (chassis + slots + inputs + outputs + ip_interfaces + alarms + card
//! status + coder services + audio profiles + …) in a single payload.
//!
//! Per-slot Xger pollers emit synthetic Critical / Minor events when
//! broadcast-critical signals flip (PTP lock lost/regained, SFP low optical
//! RX power, SFP over-temperature). These event edges are computed against
//! the prior `cardStatus` snapshot — the threshold crossing is only emitted
//! on the edge, so steady-state alerts do not flood the manager.

use anyhow::Result;
use bilbycast_gateway_sdk::{Emitter, EventSeverity, GatewayEvent, GatewayTargetHealth};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::capabilities::DeviceCapabilities;
use super::jsonrpc::JsonRpcClient;
use super::reachability::{
    classify_jsonrpc_error, detect_egress_ip, ReachabilityState, TransitionOutcome,
};
use super::state::SharedAppearXState;
use crate::config::PollingConfig;
use crate::event_gate::{EventGate, GateDecision};

/// Static identity of the gateway sidecar. Built once from the operator's
/// TOML config plus a hostname probe at startup, then threaded into the
/// alarms poller so every health heartbeat carries the gateway's own
/// host / target / reachability metadata.
#[derive(Debug, Clone)]
pub struct GatewayIdentity {
    /// IP / hostname of the Appear X chassis we're polling.
    pub target_address: String,
    /// Sidecar's own hostname (best-effort, may be `None`).
    pub gateway_host: Option<String>,
    /// Threshold (consecutive failed polls) before flipping reachable=false.
    pub failure_threshold: u32,
    /// Dwell time (seconds) before a state flip fires a `target_*` event.
    pub event_dwell_secs: u64,
}

/// Interval (seconds) at which the consolidated stats snapshot is pushed to
/// the manager. Independent of the per-source polling cadences.
const STATS_EMIT_INTERVAL_SECS: u64 = 5;

/// Run the polling engine — spawns tasks for each poll type plus a single
/// stats emitter that ships a consolidated snapshot to the manager.
///
/// `caps` is the result of [`crate::appear_x::capabilities::discover`] and
/// drives which per-slot tasks are spawned. Per-slot polling is only set up
/// for slots whose discovery actually found matching modules; slots whose
/// card software uses an unknown namespace silently skip per-slot polling
/// rather than spamming "Method not found".
pub async fn run_polling(
    client: JsonRpcClient,
    config: PollingConfig,
    caps: DeviceCapabilities,
    state: SharedAppearXState,
    emitter: Emitter,
    identity: GatewayIdentity,
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

    spawn_alarms_poller(&client, &config, &state, &emitter, &event_gate, &identity, &cancel);
    spawn_chassis_poller(&client, &config, &state, &cancel);
    spawn_cards_poller(&client, &config, &state, &cancel);

    // Per-slot polling, gated by discovery. For each slot we walk the
    // discovered modules and only spawn pollers backed by modules this
    // firmware confirmed it speaks.
    info!(
        "Setting up per-slot polling for {} slot(s) on {} chassis",
        caps.slots.len(),
        caps.chassis_type
    );
    for (slot, slot_caps) in &caps.slots {
        let slot = *slot;
        if slot_caps.discovered_modules.is_empty() {
            warn!(
                "Slot {} ({}, software_id={:?}): no card-level modules matched the \
                 probe registry — only chassis-level polls will report data for this slot. \
                 To add support, register the firmware's namespace in \
                 src/appear_x/probe_registry.rs.",
                slot, slot_caps.name, slot_caps.software_id
            );
            continue;
        }

        // Summarise what we're about to poll on this slot so operators can
        // verify the probe registry in the log.
        let families: Vec<&str> = {
            let mut f: Vec<&str> = slot_caps
                .discovered_modules
                .values()
                .map(|r| r.family.as_str())
                .collect();
            f.sort();
            f.dedup();
            f
        };
        info!("Slot {slot}: polling families = {families:?}");

        // ── ipGateway (legacy IP Gateway boards — ME-3000 / ME-4000 family)
        if slot_caps.has_module("ipGateway", "input") {
            let v = slot_caps
                .module_version("ipGateway", "input")
                .unwrap_or("1.15")
                .to_string();
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                config.inputs_interval_secs,
                &format!("ipgw-inputs-slot{slot}"),
                move |c, st| {
                    let v = v.clone();
                    Box::pin(async move {
                        let method = format!("ipGateway:{v}/input/GetInputs");
                        let r = c.call_board(slot, &method, json!({})).await?;
                        st.set_slot_inputs(slot, r.get("data").cloned().unwrap_or(json!([])))
                            .await;
                        Ok(())
                    })
                },
            );
        }
        if slot_caps.has_module("ipGateway", "output") {
            let v = slot_caps
                .module_version("ipGateway", "output")
                .unwrap_or("1.15")
                .to_string();
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                config.outputs_interval_secs,
                &format!("ipgw-outputs-slot{slot}"),
                move |c, st| {
                    let v = v.clone();
                    Box::pin(async move {
                        let method = format!("ipGateway:{v}/output/GetOutputs");
                        let r = c.call_board(slot, &method, json!({})).await?;
                        st.set_slot_outputs(slot, r.get("data").cloned().unwrap_or(json!([])))
                            .await;
                        Ok(())
                    })
                },
            );
        }
        if slot_caps.has_module("ipGateway", "ipinterface") {
            let v = slot_caps
                .module_version("ipGateway", "ipinterface")
                .unwrap_or("1.15")
                .to_string();
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                config.inputs_interval_secs * 2,
                &format!("ipgw-ifaces-slot{slot}"),
                move |c, st| {
                    let v = v.clone();
                    Box::pin(async move {
                        let method = format!("ipGateway:{v}/ipinterface/GetIpInterfaces");
                        let r = c.call_board(slot, &method, json!({})).await?;
                        st.set_slot_ip_interfaces(
                            slot,
                            r.get("data").cloned().unwrap_or(json!([])),
                        )
                        .await;
                        Ok(())
                    })
                },
            );
        }

        // ── Xger card-manager surface (X5 / X10 / X20 chassis; commissioned
        //    IP 2110 encoders). The fast `cardStatus` poller also drives
        //    broadcast-critical synthetic events (PTP / SFP). Slow pollers
        //    only spawn for modules that responded at discovery.
        if slot_caps.has_module("Xger", "cardStatus") {
            let v = slot_caps
                .module_version("Xger", "cardStatus")
                .unwrap_or("2.55")
                .to_string();
            let emitter = emitter.clone();
            let event_gate = event_gate.clone();
            let rx_thresh = config.sfp_low_rx_dbm_threshold;
            let temp_thresh = config.sfp_high_temp_c_threshold;
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                config.card_status_interval_secs,
                &format!("xger-cardstatus-slot{slot}"),
                move |c, st| {
                    let v = v.clone();
                    let emitter = emitter.clone();
                    let event_gate = event_gate.clone();
                    Box::pin(async move {
                        let method = format!("Xger:{v}/cardStatus/GetCardStatus");
                        let r = c.call_board(slot, &method, json!({"slot": slot})).await?;
                        let prior = st.set_card_status(slot, r.clone()).await;
                        let events = derive_card_status_events(slot, prior.as_ref(), &r, rx_thresh, temp_thresh);
                        for ev in events {
                            emit_gated_event(&emitter, &event_gate, ev).await;
                        }
                        Ok(())
                    })
                },
            );
        }

        // Slow Xger config pollers — spawn only for discovered modules.
        let slow = config.xger_config_interval_secs;

        spawn_xger_slow(
            slot_caps,
            "coderService",
            "GetCoderServices",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, data| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_coder_services(slot, data).await;
                });
            },
        );
        spawn_xger_slow(
            slot_caps,
            "multiService",
            "GetMultiServices",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, data| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_multi_services(slot, data).await;
                });
            },
        );
        spawn_xger_slow(
            slot_caps,
            "audioProfile",
            "GetAudioProfiles",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, data| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_audio_profiles(slot, data).await;
                });
            },
        );
        spawn_xger_slow(
            slot_caps,
            "ipInterface",
            "GetIpInterfaces",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, data| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_xger_ip_interfaces(slot, data).await;
                });
            },
        );
        spawn_xger_slow(
            slot_caps,
            "cardAllocation",
            "GetCardAllocations",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, data| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_card_allocations(slot, data).await;
                });
            },
        );

        // Phase 2: ipConnection (UDP / RTP / SRT / RIST transport bindings).
        // Only present on commissioned units that load the `Xger:*/ipConnection`
        // module — bare X5 HEVC SDI doesn't, and `spawn_xger_slow` no-ops if
        // the module wasn't discovered.
        spawn_xger_slow(
            slot_caps,
            "ipConnection",
            "GetIpConnections",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, data| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_ip_connections(slot, data).await;
                });
            },
        );

        // Phase 2: redundancyGroup (ST 2022-7 / hot-standby) configured pairs.
        spawn_xger_slow(
            slot_caps,
            "redundancyGroup",
            "GetRedundancyGroups",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, data| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_redundancy_groups(slot, data).await;
                });
            },
        );

        // Phase 2: redundancyGroupStatus (live state — active leg, switch
        // count). Single-object return; raw blob into the slot map. Polled at
        // the same fast cadence as cardStatus so operators see leg switches
        // promptly.
        spawn_xger_slow_raw(
            slot_caps,
            "redundancyGroupStatus",
            "GetRedundancyGroupStatus",
            &client,
            &state,
            &cancel,
            config.card_status_interval_secs,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_redundancy_group_status(slot, raw).await;
                });
            },
        );

        // Phase 3a: per-encoder / per-decoder runtime config polls. Different
        // module names per family (`hipEncoder` vs `hipTsEncoder` vs
        // `hipDecoder` vs `hipTsDecoder`); each `spawn_iface_slow_raw` call
        // gates on the slot having actually discovered that module so a bare
        // X5 silently skips.
        spawn_iface_slow_raw(
            slot_caps, "hipEnc", "hipEncoder", "GetEncoders",
            &client, &state, &cancel, slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move { st.set_hip_encoders(slot, raw).await; });
            },
        );
        spawn_iface_slow_raw(
            slot_caps, "hipTsEnc", "hipTsEncoder", "GetEncoders",
            &client, &state, &cancel, slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move { st.set_hip_encoders(slot, raw).await; });
            },
        );
        spawn_iface_slow_raw(
            slot_caps, "hipDec", "hipDecoder", "GetDecoders",
            &client, &state, &cancel, slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move { st.set_hip_decoders(slot, raw).await; });
            },
        );
        spawn_iface_slow_raw(
            slot_caps, "hipTsDec", "hipTsDecoder", "GetDecoders",
            &client, &state, &cancel, slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move { st.set_hip_decoders(slot, raw).await; });
            },
        );

        // Phase 3b: SCTE-35 / DPI / ESAM splicing surface. Slow polls for
        // status modules (60 s — splicing changes infrequently); the
        // scte35LogApi history is fetched on-demand from the command handler.
        spawn_xger_slow_raw(
            slot_caps,
            "dpiStatus",
            "GetDpiStatus",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move { st.set_dpi_status(slot, raw).await; });
            },
        );
        spawn_xger_slow_raw(
            slot_caps,
            "esamStatus",
            "GetEsamStatus",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move { st.set_esam_status(slot, raw).await; });
            },
        );
        spawn_xger_slow_raw(
            slot_caps,
            "poisServerStatus",
            "GetPoisServerStatus",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move { st.set_pois_server_status(slot, raw).await; });
            },
        );

        // poolConfig / lockStatus / psiStatus — single-object returns (not
        // arrays). Use the raw `result` blob.
        spawn_xger_slow_raw(
            slot_caps,
            "poolConfig",
            "GetPoolConfig",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_pool_config(slot, raw).await;
                });
            },
        );
        spawn_xger_slow_raw(
            slot_caps,
            "lockStatus",
            "GetLockStatus",
            &client,
            &state,
            &cancel,
            config.card_status_interval_secs,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_lock_status(slot, raw).await;
                });
            },
        );
        spawn_xger_slow_raw(
            slot_caps,
            "psiStatus",
            "GetPsiStatus",
            &client,
            &state,
            &cancel,
            slow,
            |st, slot, raw| {
                let st = st.clone();
                tokio::spawn(async move {
                    st.set_psi_status(slot, raw).await;
                });
            },
        );

        // ── board:{ver}/services/GetOutputServices ───────────────────
        //
        // On the X5 / X10 / X20 card-manager firmware the `Xger:*/ipConnection`
        // module isn't loaded (the X5 HEVC SDI only exposes 7 Xger modules;
        // ipConnection is not among them). The native Appear X web UI still
        // shows IP outputs though — it pulls them from the `board:*/services`
        // module via `GetOutputServices`, which returns entries with
        // `nodeType: Flow::FlowSink::IpOutput::` and a nested `sources` list
        // pointing at the input service feeding each leg. SMPTE 2022-7
        // redundant pairs surface as two entries with the same label / name
        // and different `body.address` — the manager UI groups them visually.
        // Gate on `board/services` discovery; skip if the card only speaks
        // legacy `ipGateway` (GetOutputs already covers that path).
        if slot_caps.has_module("board", "services") {
            let v = slot_caps
                .module_version("board", "services")
                .unwrap_or("2.16")
                .to_string();
            spawn_state_poll(
                client.clone(),
                state.clone(),
                cancel.child_token(),
                slow,
                &format!("board-output-services-slot{slot}"),
                move |c, st| {
                    let v = v.clone();
                    Box::pin(async move {
                        let method = format!("board:{v}/services/GetOutputServices");
                        let r = c.call_board(slot, &method, json!({})).await?;
                        st.set_output_services(
                            slot,
                            r.get("data").cloned().unwrap_or(json!([])),
                        )
                        .await;
                        Ok(())
                    })
                },
            );
        }
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

/// Spawn the `cardStatus` independent "alarms" poller that also emits a
/// `health` envelope (with the `gateway_target` sub-status) on every tick
/// — both when the poll succeeds AND when it fails. Failure path is what
/// drives the manager-side dashboard's third "Target down" amber state.
fn spawn_alarms_poller(
    client: &JsonRpcClient,
    config: &PollingConfig,
    state: &SharedAppearXState,
    emitter: &Emitter,
    event_gate: &Arc<EventGate>,
    identity: &GatewayIdentity,
    cancel: &CancellationToken,
) {
    let alarms_method = format!("mmi:{}/alarms/GetActiveAlarms", config.alarms_mmi_version);
    let emitter = emitter.clone();
    let event_gate = event_gate.clone();
    let identity = identity.clone();
    let reachability = Arc::new(ReachabilityState::new(
        identity.failure_threshold,
        identity.event_dwell_secs,
    ));
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        config.alarms_interval_secs,
        "alarms",
        move |c, st| {
            let alarms_method = alarms_method.clone();
            let emitter = emitter.clone();
            let event_gate = event_gate.clone();
            let identity = identity.clone();
            let reachability = reachability.clone();
            Box::pin(async move {
                // Capture the JSON-RPC outcome explicitly — we need to emit
                // a health heartbeat in BOTH the success and failure paths
                // so the manager's dashboard can render the third
                // "Target down" state when the chassis is unreachable.
                let call_outcome = c.call_mmi(&alarms_method, json!({"query": {}})).await;

                // Status used in the existing health field — derived from
                // alarm severity on success; "critical" on failure (no
                // alarm visibility = treat as critical for the status
                // string, separate from gateway_target.reachable).
                let (status, alarms_value): (&'static str, Value) = match &call_outcome {
                    Ok(result) => {
                        let alarms_value = result.get("data").cloned().unwrap_or(json!([]));
                        let alarm_list: Vec<Value> =
                            alarms_value.as_array().cloned().unwrap_or_default();
                        let status = derive_status(&alarm_list);
                        let (new_alarms, cleared_ids) =
                            st.set_alarms(alarm_list, status).await;

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
                            let object_type = alarm
                                .get("configObjectType")
                                .and_then(|s| s.as_str())
                                .unwrap_or("");
                            let object_id = alarm
                                .get("configObjectId")
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
                                details["object_label"] = json!(object_label);
                            }
                            if !object_type.is_empty() {
                                details["object_type"] = json!(object_type);
                            }
                            if !object_id.is_empty() {
                                details["object_id"] = json!(object_id);
                            }
                            let event = GatewayEvent::new(event_severity, "alarm", message)
                                .with_details(details);
                            emit_gated_event(&emitter, &event_gate, event).await;
                        }

                        for alarm_id in &cleared_ids {
                            let event = GatewayEvent::info(
                                "alarm",
                                format!("Alarm cleared: {alarm_id}"),
                            )
                            .with_details(json!({ "alarm_id": alarm_id }));
                            emit_gated_event(&emitter, &event_gate, event).await;
                        }
                        (status, alarms_value)
                    }
                    Err(e) => {
                        // Verbose vendor error stays in the local log only —
                        // never on the wire (could quote URLs / credentials).
                        warn!(error = %e, "appear_x alarms poll failed");
                        ("critical", json!([]))
                    }
                };

                // Update reachability state based on outcome and decide
                // whether to fire a target_reachability event.
                let transition = match &call_outcome {
                    Ok(_) => reachability.record_success(),
                    Err(e) => {
                        let code = classify_jsonrpc_error(e);
                        reachability.record_failure(code)
                    }
                };
                match transition {
                    TransitionOutcome::BecameUnreachable {
                        consecutive_failures,
                        last_error_code,
                    } => {
                        let mut details = json!({
                            "error_code": "target_unreachable",
                            "consecutive_failures": consecutive_failures,
                            "target_address": identity.target_address,
                        });
                        if let Some(code) = last_error_code {
                            details["last_error_code"] = json!(code);
                        }
                        let event = GatewayEvent::new(
                            EventSeverity::Critical,
                            "target_reachability",
                            format!(
                                "Target Appear X at {} unreachable",
                                identity.target_address
                            ),
                        )
                        .with_details(details);
                        emit_gated_event(&emitter, &event_gate, event).await;
                    }
                    TransitionOutcome::Recovered { downtime_secs } => {
                        let event = GatewayEvent::info(
                            "target_reachability",
                            format!(
                                "Target Appear X at {} reachable again (downtime {} s)",
                                identity.target_address, downtime_secs
                            ),
                        )
                        .with_details(json!({
                            "error_code": "target_recovered",
                            "downtime_secs": downtime_secs,
                            "target_address": identity.target_address,
                        }));
                        emit_gated_event(&emitter, &event_gate, event).await;
                    }
                    TransitionOutcome::NoChange => {}
                }

                // Build the gateway_target sub-status and emit the health
                // heartbeat unconditionally — both success and failure
                // paths reach here.
                let target = GatewayTargetHealth {
                    reachable: reachability.is_reachable(),
                    target_address: identity.target_address.clone(),
                    gateway_host: identity.gateway_host.clone(),
                    gateway_egress_ip: detect_egress_ip(),
                    last_successful_poll_unix: reachability.last_success_unix(),
                    last_error_code: reachability.last_error_code(),
                    consecutive_failures: Some(reachability.consecutive_failures()),
                };
                let health = json!({
                    "status": status,
                    "alarms": alarms_value,
                    "version": env!("CARGO_PKG_VERSION"),
                });
                let _ = emitter.emit_health_with_target(health, target).await;

                // Surface the original error to spawn_state_poll's logger
                // on failure paths so we keep the existing observability.
                call_outcome.map(|_| ())
            })
        },
    );
}

fn spawn_chassis_poller(
    client: &JsonRpcClient,
    config: &PollingConfig,
    state: &SharedAppearXState,
    cancel: &CancellationToken,
) {
    let method = format!("mmi:{}/chassisModel/GetGraph", config.chassis_mmi_version);
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        config.chassis_interval_secs,
        "chassis",
        move |c, st| {
            let method = method.clone();
            Box::pin(async move {
                let r = c.call_mmi(&method, json!({})).await?;
                st.set_chassis(r).await;
                Ok(())
            })
        },
    );
}

fn spawn_cards_poller(
    client: &JsonRpcClient,
    config: &PollingConfig,
    state: &SharedAppearXState,
    cancel: &CancellationToken,
) {
    let info_m = format!("mmi:{}/cards/GetChassisInfo", config.cards_mmi_version);
    let states_m = format!("mmi:{}/cards/GetCardStates", config.cards_mmi_version);
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        config.cards_interval_secs,
        "cards",
        move |c, st| {
            let info_m = info_m.clone();
            let states_m = states_m.clone();
            Box::pin(async move {
                let info = c.call_mmi(&info_m, json!({})).await?;
                let states = c.call_mmi(&states_m, json!({})).await?;
                st.set_cards(
                    info.get("data").cloned().unwrap_or(json!({})),
                    states.get("cards").cloned().unwrap_or(json!([])),
                )
                .await;
                Ok(())
            })
        },
    );
}

/// Spawn a per-slot Xger poller that extracts `result.data` as an array and
/// calls the provided setter. No-op if the slot didn't discover the module.
fn spawn_xger_slow<F>(
    slot_caps: &super::capabilities::SlotCapabilities,
    module: &'static str,
    command: &'static str,
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    cancel: &CancellationToken,
    interval_secs: u64,
    apply: F,
) where
    F: Fn(SharedAppearXState, u32, Value) + Send + Sync + 'static + Clone,
{
    let slot = slot_caps.slot;
    let version = match slot_caps.module_version("Xger", module) {
        Some(v) => v.to_string(),
        None => return,
    };
    let apply = apply.clone();
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        interval_secs,
        &format!("xger-{module}-slot{slot}"),
        move |c, st| {
            let version = version.clone();
            let apply = apply.clone();
            Box::pin(async move {
                let method = format!("Xger:{version}/{module}/{command}");
                let r = c.call_board(slot, &method, json!({})).await?;
                let data = r.get("data").cloned().unwrap_or(json!([]));
                apply(st, slot, data);
                Ok(())
            })
        },
    );
}

/// Like `spawn_xger_slow` but for commands whose result is a single opaque
/// object instead of `{ data: [] }`. Gets the whole `result` blob.
fn spawn_xger_slow_raw<F>(
    slot_caps: &super::capabilities::SlotCapabilities,
    module: &'static str,
    command: &'static str,
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    cancel: &CancellationToken,
    interval_secs: u64,
    apply: F,
) where
    F: Fn(SharedAppearXState, u32, Value) + Send + Sync + 'static + Clone,
{
    let slot = slot_caps.slot;
    let version = match slot_caps.module_version("Xger", module) {
        Some(v) => v.to_string(),
        None => return,
    };
    let apply = apply.clone();
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        interval_secs,
        &format!("xger-{module}-slot{slot}"),
        move |c, st| {
            let version = version.clone();
            let apply = apply.clone();
            Box::pin(async move {
                let method = format!("Xger:{version}/{module}/{command}");
                let r = c.call_board(slot, &method, json!({})).await?;
                apply(st, slot, r);
                Ok(())
            })
        },
    );
}

/// Phase 3a/3b helper: parameterised twin of `spawn_xger_slow_raw` that
/// takes the interface name as an argument so we can poll non-Xger modules
/// (`hipEnc:*`, `hipTsEnc:*`, `hipDec:*`, `hipTsDec:*`) with the same
/// idiom. Gates on the slot having actually discovered the
/// `<interface>/<module>` pair so a chassis without that family silently
/// skips spawning the task.
fn spawn_iface_slow_raw<F>(
    slot_caps: &super::capabilities::SlotCapabilities,
    interface: &'static str,
    module: &'static str,
    command: &'static str,
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    cancel: &CancellationToken,
    interval_secs: u64,
    apply: F,
) where
    F: Fn(SharedAppearXState, u32, Value) + Send + Sync + 'static + Clone,
{
    let slot = slot_caps.slot;
    let version = match slot_caps.module_version(interface, module) {
        Some(v) => v.to_string(),
        None => return,
    };
    let apply = apply.clone();
    spawn_state_poll(
        client.clone(),
        state.clone(),
        cancel.child_token(),
        interval_secs,
        &format!("{interface}-{module}-slot{slot}"),
        move |c, st| {
            let version = version.clone();
            let apply = apply.clone();
            Box::pin(async move {
                let method = format!("{interface}:{version}/{module}/{command}");
                let r = c.call_board(slot, &method, json!({})).await?;
                apply(st, slot, r);
                Ok(())
            })
        },
    );
}

/// Compare previous and current `cardStatus` snapshots for a single slot and
/// emit synthetic Critical / Minor events on threshold crossings. Only the
/// *edges* fire — a port that has been below the RX-power threshold for 10
/// minutes does NOT emit an event every poll tick.
///
/// Detections:
/// - PTP `LOCKED` → anything else  (Critical `ptp_lost`)
/// - PTP anything else → `LOCKED`  (Info `ptp_locked`)
/// - SFP RX dBm crossing below `rx_thresh` while optic is present (Minor `sfp_low_rx`)
/// - SFP temp crossing above `temp_thresh`                            (Minor `sfp_high_temperature`)
fn derive_card_status_events(
    slot: u32,
    prev: Option<&Value>,
    curr: &Value,
    rx_thresh: f64,
    temp_thresh: f64,
) -> Vec<GatewayEvent> {
    let mut events = Vec::new();

    let prev_ptp = prev.and_then(|p| ptp_state_of(p));
    let curr_ptp = ptp_state_of(curr);
    let (prev_locked, curr_locked) = (prev_ptp.as_deref() == Some("LOCKED"), curr_ptp.as_deref() == Some("LOCKED"));
    if prev.is_some() && prev_locked && !curr_locked {
        events.push(
            GatewayEvent::new(
                EventSeverity::Critical,
                "ptp",
                format!("PTP lock lost on slot {slot} (state={})", curr_ptp.clone().unwrap_or_else(|| "unknown".into())),
            )
            .with_details(json!({ "slot": slot, "state": curr_ptp, "error_code": "ptp_lost" })),
        );
    } else if prev.is_some() && !prev_locked && curr_locked {
        events.push(
            GatewayEvent::info(
                "ptp",
                format!("PTP locked on slot {slot}"),
            )
            .with_details(json!({ "slot": slot, "state": "LOCKED" })),
        );
    }

    let prev_rx = prev.and_then(min_rx_dbm);
    let curr_rx = min_rx_dbm(curr);
    if let Some(curr_min) = curr_rx {
        let was_below = prev_rx.map(|p| p < rx_thresh).unwrap_or(false);
        let now_below = curr_min < rx_thresh;
        if now_below && !was_below {
            events.push(
                GatewayEvent::new(
                    EventSeverity::Minor,
                    "sfp",
                    format!(
                        "SFP RX optical power on slot {slot} is below {rx_thresh:.1} dBm (current min {curr_min:.1} dBm)"
                    ),
                )
                .with_details(json!({
                    "slot": slot,
                    "rx_power_dbm_min": curr_min,
                    "threshold_dbm": rx_thresh,
                    "error_code": "sfp_low_rx_power",
                })),
            );
        } else if !now_below && was_below {
            events.push(
                GatewayEvent::info(
                    "sfp",
                    format!("SFP RX optical power recovered on slot {slot} (current min {curr_min:.1} dBm)"),
                )
                .with_details(json!({ "slot": slot, "rx_power_dbm_min": curr_min })),
            );
        }
    }

    let prev_temp = prev.and_then(max_temp);
    let curr_temp = max_temp(curr);
    if let Some(curr_max) = curr_temp {
        let was_over = prev_temp.map(|p| p > temp_thresh).unwrap_or(false);
        let now_over = curr_max > temp_thresh;
        if now_over && !was_over {
            events.push(
                GatewayEvent::new(
                    EventSeverity::Minor,
                    "sfp",
                    format!(
                        "SFP cage temperature on slot {slot} exceeded {temp_thresh:.0} °C (current max {curr_max:.1} °C)"
                    ),
                )
                .with_details(json!({
                    "slot": slot,
                    "temp_c_max": curr_max,
                    "threshold_c": temp_thresh,
                    "error_code": "sfp_high_temperature",
                })),
            );
        } else if !now_over && was_over {
            events.push(
                GatewayEvent::info(
                    "sfp",
                    format!("SFP cage temperature recovered on slot {slot} (current max {curr_max:.1} °C)"),
                )
                .with_details(json!({ "slot": slot, "temp_c_max": curr_max })),
            );
        }
    }

    events
}

fn ptp_state_of(status: &Value) -> Option<String> {
    let pl = status.get("ptpLock")?;
    if pl.is_object() && pl.as_object().map(|o| o.is_empty()).unwrap_or(false) {
        return None;
    }
    pl.get("state")
        .or_else(|| pl.pointer("/value/state"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn min_rx_dbm(status: &Value) -> Option<f64> {
    let mut m: Option<f64> = None;
    let qsfp = status.pointer("/qsfpStatus/value").and_then(|v| v.as_array());
    let sfp = status.pointer("/sfpStatus/value").and_then(|v| v.as_array());
    for arr in [qsfp, sfp].into_iter().flatten() {
        for entry in arr {
            if let Some(rx) = entry.pointer("/value/diagnostics/value/rxPwr").and_then(|v| v.as_array()) {
                for x in rx {
                    if let Some(mw) = x.as_f64() {
                        if mw > 0.0 {
                            let dbm = 10.0 * mw.log10();
                            m = Some(match m { Some(v) => v.min(dbm), None => dbm });
                        }
                    }
                }
            }
        }
    }
    m
}

fn max_temp(status: &Value) -> Option<f64> {
    let mut m: Option<f64> = None;
    let qsfp = status.pointer("/qsfpStatus/value").and_then(|v| v.as_array());
    let sfp = status.pointer("/sfpStatus/value").and_then(|v| v.as_array());
    for arr in [qsfp, sfp].into_iter().flatten() {
        for entry in arr {
            if let Some(t) = entry.pointer("/value/diagnostics/value/temp").and_then(|v| v.as_f64()) {
                m = Some(match m { Some(v) => v.max(t), None => t });
            }
        }
    }
    m
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
            let _ = emitter.emit_event(summary).await;
            let _ = emitter.emit_event(event).await;
        }
        GateDecision::Suppress { summary } => {
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
