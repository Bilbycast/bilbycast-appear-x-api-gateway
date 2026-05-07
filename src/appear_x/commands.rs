// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Command handler that translates manager commands into Appear X JSON-RPC calls.
//!
//! Implements the SDK's [`CommandHandler`] trait — the SDK owns the WS read
//! loop, the `command_ack` serialisation, and the `get_config` →
//! `config_response` envelope dance. This module is purely the vendor
//! translation layer: `action["type"]` → JSON-RPC method.
//!
//! The command surface covers two Appear X module families:
//!
//! - **`ipGateway`** — legacy ME-3000 / ME-4000 boards (symmetrical
//!   `GetInputs`/`SetInputs`, `GetOutputs`/`SetOutputs`, `GetIpInterfaces`).
//! - **`Xger`** — card-manager surface on X5 / X10 / X20 chassis
//!   (`cardStatus`, `coderService`, `multiService`, `audioProfile`,
//!   `ipInterface`, `cardAllocation`, `poolConfig`, `lockStatus`, `psiStatus`).
//!
//! All `set_*` / `delete_*` commands use the Appear Get/Set symmetry pattern:
//! `SetFoo` and `DeleteFoo` take the same struct shape as `GetFoo` returns,
//! so the manager (or an operator) can `Get` → mutate → `Set` without
//! remapping fields.

use async_trait::async_trait;
use bilbycast_gateway_sdk::upgrade::{error_codes as upgrade_error_codes, UpgradeCoordinator};
use bilbycast_gateway_sdk::{CommandError, CommandHandler};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use super::jsonrpc::JsonRpcClient;
use super::state::SharedAppearXState;
use crate::config::PollingConfig;

/// MMI interface versions used by on-demand command calls. Threaded in from
/// [`PollingConfig`] so the command path uses the same firmware-specific
/// versions as the polling path (different Appear firmware exposes
/// `chassisModel` at 2.16, 4.1, etc. — see polling.rs).
#[derive(Debug, Clone)]
pub struct MmiVersions {
    pub alarms: String,
    pub chassis: String,
}

impl From<&PollingConfig> for MmiVersions {
    fn from(p: &PollingConfig) -> Self {
        Self {
            alarms: p.alarms_mmi_version.clone(),
            chassis: p.chassis_mmi_version.clone(),
        }
    }
}

/// Vendor-side command handler, wired up to the SDK via [`CommandHandler`].
pub struct AppearXCommandHandler {
    client: JsonRpcClient,
    state: SharedAppearXState,
    mmi: MmiVersions,
    /// Optional remote-upgrade coordinator. `None` when the operator left
    /// `[upgrade]` out of the TOML; the `upgrade_binary` arm then returns
    /// `upgrade_disabled`. Mirrors the edge's `global_coordinator()`
    /// pattern but kept on the handler so the SDK's stateless dispatch
    /// model holds.
    upgrade_coord: Option<Arc<UpgradeCoordinator>>,
}

impl AppearXCommandHandler {
    pub fn new(
        client: JsonRpcClient,
        state: SharedAppearXState,
        mmi: MmiVersions,
        upgrade_coord: Option<Arc<UpgradeCoordinator>>,
    ) -> Self {
        Self {
            client,
            state,
            mmi,
            upgrade_coord,
        }
    }
}

/// Wrapper handler used during the startup window where the manager WS is up
/// but Appear X capability discovery hasn't completed yet (chassis powered
/// down, on the wrong subnet, etc.). The sidecar registers this immediately
/// so the manager can render the node as Online with `gateway_target.reachable
/// = false`; once discovery succeeds the real [`AppearXCommandHandler`] is
/// installed and every command thereafter forwards to it.
///
/// While the inner handler is `None`, vendor commands return a
/// `discovery_in_progress` `CommandError` so the manager UI can show a
/// "chassis unreachable" banner instead of a generic timeout.
///
/// `upgrade_binary` is a deliberate exception — it bypasses the inner-handler
/// check and goes straight to the upgrade coordinator. Sidecar self-upgrade
/// must work even when the chassis is unreachable, otherwise an operator
/// can't ship a fix to a sidecar whose target is offline.
pub struct DeferredAppearXHandler {
    inner: RwLock<Option<Arc<AppearXCommandHandler>>>,
    /// Cloned-from-startup. Same `Option<Arc<UpgradeCoordinator>>` as the
    /// inner real handler so `upgrade_binary` works pre- and post-discovery.
    upgrade_coord: Option<Arc<UpgradeCoordinator>>,
}

impl DeferredAppearXHandler {
    pub fn new(upgrade_coord: Option<Arc<UpgradeCoordinator>>) -> Self {
        Self {
            inner: RwLock::new(None),
            upgrade_coord,
        }
    }

    /// Swap the real handler in. Idempotent — second call is a no-op.
    pub async fn install(&self, handler: Arc<AppearXCommandHandler>) {
        let mut g = self.inner.write().await;
        if g.is_none() {
            *g = Some(handler);
        }
    }
}

#[async_trait]
impl CommandHandler for DeferredAppearXHandler {
    async fn handle_command(
        &self,
        command_id: String,
        action: Value,
    ) -> Result<Value, CommandError> {
        // Sidecar self-upgrade bypasses the inner-handler gate so a
        // chassis-down event doesn't lock operators out of fixing a
        // sidecar bug.
        if action.get("type").and_then(|t| t.as_str()) == Some("upgrade_binary") {
            return dispatch_upgrade_binary(&self.upgrade_coord, &action).await;
        }
        let h = self.inner.read().await.clone();
        match h {
            Some(h) => h.handle_command(command_id, action).await,
            None => Err(CommandError::new(
                "discovery_in_progress",
                "Appear X capability discovery has not completed — the chassis may be unreachable. \
                 Commands will be accepted once the sidecar can talk to the unit.",
            )),
        }
    }

    async fn on_config_request(&self) -> Value {
        let h = self.inner.read().await.clone();
        match h {
            Some(h) => h.on_config_request().await,
            None => json!({ "discovery_in_progress": true }),
        }
    }
}

#[async_trait]
impl CommandHandler for AppearXCommandHandler {
    /// Translate one manager command into the corresponding Appear X
    /// JSON-RPC call. The SDK dispatches `get_config` separately via
    /// [`CommandHandler::on_config_request`] — it never lands here.
    async fn handle_command(
        &self,
        _command_id: String,
        action: Value,
    ) -> Result<Value, CommandError> {
        let action_type = action
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| CommandError::validation("missing action.type"))?;

        let slot = action.get("slot").and_then(|s| s.as_u64()).unwrap_or(1) as u32;

        info!("Handling command: {} (slot {})", action_type, slot);

        match action_type {
            // ── ipGateway family (legacy IP Gateway boards / ME-3000 / ME-4000) ──
            //
            // These methods only exist on cards that expose the `ipGateway`
            // interface. Xger-only chassis (X5 HEVC SDI, X5 JPEG-XS, etc.)
            // do NOT speak `ipGateway:*` — inputs / outputs on those cards
            // are modeled via `coder_services` and `multi_services` on the
            // card-manager surface. Gate each command on discovery so the
            // operator gets a clear error instead of a raw "Method not found"
            // from the Appear unit.
            "get_inputs" => {
                let ver = require_ipgw(&self.state, slot, "input", "get_inputs")?;
                self.client
                    .call_board(
                        slot,
                        &format!("ipGateway:{ver}/input/GetInputs"),
                        json!({}),
                    )
                    .await
                    .map_err(vendor_error)
            }
            "get_outputs" => {
                let ver = require_ipgw(&self.state, slot, "output", "get_outputs")?;
                self.client
                    .call_board(
                        slot,
                        &format!("ipGateway:{ver}/output/GetOutputs"),
                        json!({}),
                    )
                    .await
                    .map_err(vendor_error)
            }
            "get_services" => {
                // `services` lives on the `board` interface, NOT `ipGateway`.
                // Gate on `board/services` discovery.
                let ver = self
                    .state
                    .discovered_version(slot, "board", "services")
                    .ok_or_else(|| unsupported_on_card(slot, "get_services", "board/services"))?;
                self.client
                    .call_board(
                        slot,
                        &format!("board:{ver}/services/GetInputServices"),
                        json!({"query": {}}),
                    )
                    .await
                    .map_err(vendor_error)
            }
            "get_ip_interfaces" => {
                let ver = require_ipgw(&self.state, slot, "ipinterface", "get_ip_interfaces")?;
                self.client
                    .call_board(
                        slot,
                        &format!("ipGateway:{ver}/ipinterface/GetIpInterfaces"),
                        json!({}),
                    )
                    .await
                    .map_err(vendor_error)
            }
            "set_ip_input" => {
                let ver = require_ipgw(&self.state, slot, "input", "set_ip_input")?;
                let inputs = require_field(&action, "inputs")?;
                debug!(
                    "SetInputs on slot {}: {} inputs",
                    slot,
                    inputs.as_array().map(|a| a.len()).unwrap_or(0)
                );
                self.client
                    .call_board(
                        slot,
                        &format!("ipGateway:{ver}/input/SetInputs"),
                        json!({"data": inputs}),
                    )
                    .await
                    .map_err(vendor_error)
            }
            "set_ip_output" => {
                let ver = require_ipgw(&self.state, slot, "output", "set_ip_output")?;
                let outputs = require_field(&action, "outputs")?;
                debug!(
                    "SetOutputs on slot {}: {} outputs",
                    slot,
                    outputs.as_array().map(|a| a.len()).unwrap_or(0)
                );
                self.client
                    .call_board(
                        slot,
                        &format!("ipGateway:{ver}/output/SetOutputs"),
                        json!({"data": outputs}),
                    )
                    .await
                    .map_err(vendor_error)
            }

            // ── MMI cross-card family ──
            //
            // MMI interface versions vary by firmware (`chassisModel` is 2.16
            // on older units, 4.1 on current X5/X10 firmware; alarms is 2.8
            // or 2.16). Use the operator-supplied versions threaded in from
            // PollingConfig so the command path matches what polling uses.
            "get_alarms" => self
                .client
                .call_mmi(
                    &format!("mmi:{}/alarms/GetActiveAlarms", self.mmi.alarms),
                    json!({"query": {}}),
                )
                .await
                .map_err(vendor_error),
            "get_chassis" => self
                .client
                .call_mmi(
                    &format!("mmi:{}/chassisModel/GetGraph", self.mmi.chassis),
                    json!({}),
                )
                .await
                .map_err(vendor_error),

            // ── Xger card-manager family (X5 / X10 / X20) ──
            "get_card_status" => xger_call(
                &self.client,
                &self.state,
                slot,
                "cardStatus",
                "GetCardStatus",
                json!({"slot": slot}),
            )
            .await,
            "get_coder_services" => xger_call(
                &self.client,
                &self.state,
                slot,
                "coderService",
                "GetCoderServices",
                json!({}),
            )
            .await,
            "get_multi_services" => xger_call(
                &self.client,
                &self.state,
                slot,
                "multiService",
                "GetMultiServices",
                json!({}),
            )
            .await,
            "get_audio_profiles" => xger_call(
                &self.client,
                &self.state,
                slot,
                "audioProfile",
                "GetAudioProfiles",
                json!({}),
            )
            .await,
            "get_xger_ip_interfaces" => xger_call(
                &self.client,
                &self.state,
                slot,
                "ipInterface",
                "GetIpInterfaces",
                json!({}),
            )
            .await,
            "get_card_allocations" => xger_call(
                &self.client,
                &self.state,
                slot,
                "cardAllocation",
                "GetCardAllocations",
                json!({}),
            )
            .await,
            "get_pool_config" => xger_call(
                &self.client,
                &self.state,
                slot,
                "poolConfig",
                "GetPoolConfig",
                json!({}),
            )
            .await,
            "get_lock_status" => xger_call(
                &self.client,
                &self.state,
                slot,
                "lockStatus",
                "GetLockStatus",
                json!({}),
            )
            .await,
            "get_psi_status" => xger_call(
                &self.client,
                &self.state,
                slot,
                "psiStatus",
                "GetPsiStatus",
                json!({}),
            )
            .await,
            "get_images" => xger_call(
                &self.client,
                &self.state,
                slot,
                "imageUpload",
                "GetImages",
                json!({}),
            )
            .await,

            // Xger write commands — all Get/Set symmetrical. The Appear API
            // shape is `{data: <map<UUID, T>>}`; the manager UI ships
            // `[{key, value}]` to keep round-trip symmetry with the flattened
            // GET-side snapshot. `xger_set_keyed` folds the array back into
            // the map shape the chassis expects.
            "set_coder_services" => xger_set_keyed(
                &self.client,
                &self.state,
                slot,
                "coderService",
                "SetCoderServices",
                &action,
                "coder_services",
            )
            .await,
            "delete_coder_services" => xger_delete(
                &self.client,
                &self.state,
                slot,
                "coderService",
                "DeleteCoderServices",
                &action,
            )
            .await,
            "set_multi_services" => xger_set_keyed(
                &self.client,
                &self.state,
                slot,
                "multiService",
                "SetMultiServices",
                &action,
                "multi_services",
            )
            .await,
            "delete_multi_services" => xger_delete(
                &self.client,
                &self.state,
                slot,
                "multiService",
                "DeleteMultiServices",
                &action,
            )
            .await,
            "set_audio_profiles" => xger_set_keyed(
                &self.client,
                &self.state,
                slot,
                "audioProfile",
                "SetAudioProfiles",
                &action,
                "audio_profiles",
            )
            .await,
            "delete_audio_profiles" => xger_delete(
                &self.client,
                &self.state,
                slot,
                "audioProfile",
                "DeleteAudioProfiles",
                &action,
            )
            .await,
            "set_xger_ip_interfaces" => xger_set_keyed(
                &self.client,
                &self.state,
                slot,
                "ipInterface",
                "SetIpInterfaces",
                &action,
                "ip_interfaces",
            )
            .await,
            "delete_xger_ip_interfaces" => xger_delete(
                &self.client,
                &self.state,
                slot,
                "ipInterface",
                "DeleteIpInterfaces",
                &action,
            )
            .await,
            "set_card_allocations" => xger_set_keyed(
                &self.client,
                &self.state,
                slot,
                "cardAllocation",
                "SetCardAllocations",
                &action,
                "card_allocations",
            )
            .await,
            "set_pool_config" => xger_set_raw(
                &self.client,
                &self.state,
                slot,
                "poolConfig",
                "SetPoolConfig",
                &action,
                "pool_config",
            )
            .await,

            // Phase 2: ipConnection. Per Xger:2.55 §27 the IpConnection struct
            // is `{label, connection, dejitter?, standard, nmosEnable}` where
            // `standard` is `SMPTE_2022 | SMPTE_2110 | MPEG_TS` — there is NO
            // free transport-protocol selector (UDP/RTP are implicit per the
            // chosen standard). SRT and RIST are not part of this surface.
            "get_ip_connections" => xger_call(
                &self.client,
                &self.state,
                slot,
                "ipConnection",
                "GetIpConnections",
                json!({}),
            )
            .await,
            "set_ip_connections" => xger_set_keyed(
                &self.client,
                &self.state,
                slot,
                "ipConnection",
                "SetIpConnections",
                &action,
                "ip_connections",
            )
            .await,
            "delete_ip_connections" => xger_delete(
                &self.client,
                &self.state,
                slot,
                "ipConnection",
                "DeleteIpConnections",
                &action,
            )
            .await,

            // Phase 2: redundancyGroup (ST 2022-7 / hot-standby).
            "get_redundancy_groups" => xger_call(
                &self.client,
                &self.state,
                slot,
                "redundancyGroup",
                "GetRedundancyGroups",
                json!({}),
            )
            .await,
            "set_redundancy_groups" => xger_set_keyed(
                &self.client,
                &self.state,
                slot,
                "redundancyGroup",
                "SetRedundancyGroups",
                &action,
                "redundancy_groups",
            )
            .await,
            "delete_redundancy_groups" => xger_delete(
                &self.client,
                &self.state,
                slot,
                "redundancyGroup",
                "DeleteRedundancyGroups",
                &action,
            )
            .await,
            "get_redundancy_group_status" => xger_call(
                &self.client,
                &self.state,
                slot,
                "redundancyGroupStatus",
                "GetRedundancyGroupStatus",
                json!({}),
            )
            .await,

            // ── Phase 3a: per-encoder / per-decoder runtime config ──
            //
            // Get/Set on `hip*Encoder` / `hip*Decoder` modules. Each
            // command auto-routes on the slot's discovered family — JPEG-XS
            // (`hipEnc` / `hipDec`) vs HEVC-TS (`hipTsEnc` / `hipTsDec`) —
            // mirroring the existing `clear_all_counters` selector pattern.
            // Operator pastes a Get response, edits, sends Set. The vendor
            // body shape varies by firmware; pass-through JSON.
            "get_hip_encoders" => hip_call(&self.client, &self.state, slot,
                /*encoder=*/true, /*set=*/false, &action).await,
            "set_hip_encoders" => hip_call(&self.client, &self.state, slot,
                /*encoder=*/true, /*set=*/true, &action).await,
            "get_hip_decoders" => hip_call(&self.client, &self.state, slot,
                /*encoder=*/false, /*set=*/false, &action).await,
            "set_hip_decoders" => hip_call(&self.client, &self.state, slot,
                /*encoder=*/false, /*set=*/true, &action).await,

            // ── Phase 3b: SCTE-35 / DPI / ESAM ──
            //
            // Symmetrical Get/Set on each module (Xger surface). The
            // splice-history fetch is on-demand only — never polled, since
            // the log can be large.
            "get_dpi" => xger_call(&self.client, &self.state, slot,
                "dpi", "GetDpi", json!({})).await,
            "set_dpi" => xger_set_raw(&self.client, &self.state, slot,
                "dpi", "SetDpi", &action, "dpi").await,
            "get_dpi_status" => xger_call(&self.client, &self.state, slot,
                "dpiStatus", "GetDpiStatus", json!({})).await,
            "get_esam_config" => xger_call(&self.client, &self.state, slot,
                "esamConfig", "GetEsamConfig", json!({})).await,
            "set_esam_config" => xger_set_raw(&self.client, &self.state, slot,
                "esamConfig", "SetEsamConfig", &action, "esam_config").await,
            "get_esam_status" => xger_call(&self.client, &self.state, slot,
                "esamStatus", "GetEsamStatus", json!({})).await,
            "get_scte35_config" => xger_call(&self.client, &self.state, slot,
                "scte35Config", "GetScte35Config", json!({})).await,
            "set_scte35_config" => xger_set_raw(&self.client, &self.state, slot,
                "scte35Config", "SetScte35Config", &action, "scte35_config").await,
            "get_scte35_history" => {
                // On-demand fetch from the splice log API. Optional `limit`
                // / `since_pts` params get forwarded if present.
                let mut params = json!({});
                if let Some(p) = action.as_object() {
                    if let Some(l) = p.get("limit") { params["limit"] = l.clone(); }
                    if let Some(s) = p.get("since_pts") { params["since_pts"] = s.clone(); }
                }
                xger_call(&self.client, &self.state, slot,
                    "scte35LogApi", "GetScte35History", params).await
            },
            "get_pois_server_status" => xger_call(&self.client, &self.state, slot,
                "poisServerStatus", "GetPoisServerStatus", json!({})).await,

            "clear_all_counters" => {
                // Reset packet / sequence error counters on the encoder. The
                // Appear X API exposes `ClearAllCounters` on the
                // `hipEncStatus` module — which rides on either the
                // `hipTsEnc` (HEVC-TS) or `hipEnc` (JPEG-XS) card interface.
                // It is NOT on the Xger card-manager surface, despite
                // `Xger` being the place we do most slot-level probing.
                //
                // Route based on which encoder interface this slot actually
                // exposes; fail loudly if neither is present (e.g. on a
                // bare X5 HEVC SDI that has no commissioned encoder pool).
                let (iface, ver) = if let Some(v) =
                    self.state.discovered_version(slot, "hipTsEnc", "hipEncStatus")
                {
                    ("hipTsEnc", v)
                } else if let Some(v) =
                    self.state.discovered_version(slot, "hipEnc", "hipEncStatus")
                {
                    ("hipEnc", v)
                } else {
                    return Err(CommandError::new(
                        "unsupported_on_card",
                        format!(
                            "clear_all_counters requires a hipTsEnc or hipEnc encoder on slot {slot}. \
                             This slot exposes no encoder-status module; commission an encoder service first."
                        ),
                    ));
                };
                self.client
                    .call_board(
                        slot,
                        &format!("{iface}:{ver}/hipEncStatus/ClearAllCounters"),
                        json!({}),
                    )
                    .await
                    .map_err(vendor_error)
            }

            // ── Remote binary upgrade ──
            //
            // Same surface as `bilbycast-edge`'s `upgrade_binary` arm. Fully
            // implemented inside the SDK's `UpgradeCoordinator`; this handler
            // just validates the action shape and on success queues the
            // exit-after-drain so systemd respawns into the new binary via
            // the `current/` symlink.
            //
            // Routed via `dispatch_upgrade_binary` so the deferred handler
            // and the real handler share one implementation — sidecar
            // self-upgrade must work even when the chassis is unreachable.
            "upgrade_binary" => dispatch_upgrade_binary(&self.upgrade_coord, &action).await,

            other => Err(CommandError::unknown_action(other)),
        }
    }

    /// Respond to `get_config` with the consolidated state snapshot the
    /// polling engine maintains. The SDK wraps this in a `config_response`
    /// envelope and acks the original command.
    async fn on_config_request(&self) -> Value {
        self.state.snapshot().await
    }
}

/// Require that the slot exposes `ipGateway/<module>` and return its discovered
/// version. On cards that only speak the Xger card-manager surface (X5 HEVC
/// SDI, X5 JPEG-XS, IP 2110 encoders before commissioning) there is no
/// `ipGateway:*` interface at all, and calling `ipGateway:1.15/*/Get*` blind
/// returns a raw "Method not found" from the unit that operators then see in
/// the UI. Returning [`CommandError::new("unsupported_on_card", …)`] surfaces
/// a self-explanatory message + `error_code` taxonomy the manager UI can
/// machine-match on.
fn require_ipgw(
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    action_name: &str,
) -> Result<String, CommandError> {
    state
        .discovered_version(slot, "ipGateway", module)
        .ok_or_else(|| unsupported_on_card(slot, action_name, &format!("ipGateway/{module}")))
}

/// Dispatch `{type: "upgrade_binary", version, channel, target_arch?, variant?}`.
///
/// Mirrors the edge's `manager/client.rs::execute_command` arm at line 3406
/// but parameterised through the SDK's `UpgradeCoordinator`. The helper
/// runs in two phases:
///
/// 1. Validate the action shape (mandatory `version` + `channel`, optional
///    `target_arch` + `variant`). Mandatory-field errors lift onto
///    `command_ack.error_code` so the manager UI can target a specific
///    form field.
/// 2. Call `coord.stage(...)`. On success, schedule a 5 s drain then
///    `std::process::exit(0)` so systemd respawns into the staged
///    binary via the `current/` symlink. The ack is returned before the
///    exit fires (the deferred ack races the exit; tokio resolves the
///    ack first ~99 % of the time, and the manager treats a missed ack
///    the same way — the new gateway re-authenticates with the new
///    `software_version` on its first beat after respawn).
///
/// Shared between [`DeferredAppearXHandler`] (sidecar self-upgrade
/// works pre-discovery) and [`AppearXCommandHandler`] (post-discovery).
async fn dispatch_upgrade_binary(
    upgrade_coord: &Option<Arc<UpgradeCoordinator>>,
    action: &Value,
) -> Result<Value, CommandError> {
    let coord = upgrade_coord.as_ref().ok_or_else(|| {
        CommandError::new(
            upgrade_error_codes::UPGRADE_DISABLED,
            "remote upgrades not configured on this gateway — add an [upgrade] section to config.toml",
        )
    })?;
    let version = action.get("version").and_then(|v| v.as_str()).ok_or_else(|| {
        CommandError::new(
            upgrade_error_codes::UPGRADE_VERSION_INVALID,
            "upgrade_binary: missing 'version'",
        )
    })?;
    let channel = action.get("channel").and_then(|v| v.as_str()).ok_or_else(|| {
        CommandError::new(
            upgrade_error_codes::UPGRADE_CHANNEL_NOT_ALLOWED,
            "upgrade_binary: missing 'channel'",
        )
    })?;
    let target_arch = action.get("target_arch").and_then(|v| v.as_str());
    let variant = action.get("variant").and_then(|v| v.as_str());

    let staged = coord
        .stage(version, channel, target_arch, variant)
        .await
        .map_err(|e| CommandError::new(e.code, e.message))?;

    let from_v = staged.from_version.clone();
    let to_v = staged.to_version.clone();
    tokio::spawn(async move {
        info!("upgrade staged ({from_v} → {to_v}); draining in 5 s before respawn");
        // 5 s drain so the WS write task flushes the ack we returned
        // below and any in-flight JSON-RPC call to the chassis can
        // complete its response handler.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        std::process::exit(0);
    });

    Ok(json!({
        "status": "staged",
        "from_version": staged.from_version,
        "to_version": staged.to_version,
        "channel": staged.channel,
        "variant": staged.variant,
        "arch": staged.arch,
    }))
}

/// Build the canonical "this slot's card family doesn't expose the required
/// interface" error used by `require_ipgw` and the cross-board `services`
/// gate. Shared so every unsupported path produces the same `error_code` +
/// message shape.
fn unsupported_on_card(slot: u32, action: &str, iface_module: &str) -> CommandError {
    CommandError::new(
        "unsupported_on_card",
        format!(
            "{action} requires the `{iface_module}` module on slot {slot}. \
             This card family does not expose that interface — X5 / X10 / X20 \
             chassis speak only the Xger card-manager surface. Use the Coder \
             Services / Multi Services / Audio Profiles sections for the \
             Xger-native signal path, or run this command against a legacy \
             IP Gateway board (ME-3000 / ME-4000)."
        ),
    )
}

/// Resolve the Xger interface version for a specific module. Strict — if
/// the probe registry didn't discover this module on the slot, return an
/// `unsupported_on_card` error rather than letting the call go to the unit
/// and surfacing a raw "Method not found" RPC error to the operator.
///
/// Symmetric with [`require_ipgw`] for the legacy IP Gateway surface: every
/// command path is gated on the same discovery state the polling layer uses.
fn xger_version(
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    action_name: &str,
) -> Result<String, CommandError> {
    state
        .discovered_version(slot, "Xger", module)
        .ok_or_else(|| unsupported_on_card(slot, action_name, &format!("Xger/{module}")))
}

/// Issue a plain Xger Get call and return the raw JSON-RPC result. Gated on
/// per-slot capability discovery: the call never leaves the gateway if the
/// probe registry didn't find this module on the slot.
async fn xger_call(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    params: Value,
) -> Result<Value, CommandError> {
    let action_name = command_to_action(command);
    let ver = xger_version(state, slot, module, &action_name)?;
    client
        .call_board(slot, &format!("Xger:{ver}/{module}/{command}"), params)
        .await
        .map_err(vendor_error)
}

/// Issue an Xger Set command whose API shape is `{data: <map<UUID, T>>}`.
///
/// The manager UI sends `[{key: "<uuid>", value: <T>}]` to keep round-trip
/// symmetry with the flattened GET response shape exposed in the snapshot.
/// This helper folds that array into the API's UUID-keyed map.
///
/// Per the Xger:2.55 spec (e.g. §28.2.2 ipInterface/SetIpInterfaces): "Interface
/// uuids are deterministically calculated. The uuids used in the RPC Request are
/// ignored." Other map-shaped Set commands behave the same — the chassis treats
/// the supplied UUID as opaque, so the manager-side UUID is preserved on the
/// wire for clean round-trip even though the chassis may re-key.
async fn xger_set_keyed(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    action: &Value,
    payload_key: &str,
) -> Result<Value, CommandError> {
    let action_name = command_to_action(command);
    let ver = xger_version(state, slot, module, &action_name)?;
    let raw = action.get(payload_key).cloned().ok_or_else(|| {
        CommandError::validation(format!("missing '{payload_key}' field").as_str())
    })?;
    let arr = raw.as_array().ok_or_else(|| {
        CommandError::validation(
            format!("'{payload_key}' must be an array of {{key, value}} objects").as_str(),
        )
    })?;
    let mut map = serde_json::Map::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| {
            CommandError::validation(
                format!("'{payload_key}[{i}]' must be an object with 'key' and 'value'").as_str(),
            )
        })?;
        let key = obj
            .get("key")
            .and_then(|k| k.as_str())
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                CommandError::validation(
                    format!("'{payload_key}[{i}].key' must be a non-empty string").as_str(),
                )
            })?;
        let value = obj.get("value").cloned().ok_or_else(|| {
            CommandError::validation(format!("'{payload_key}[{i}].value' is missing").as_str())
        })?;
        map.insert(key.to_string(), value);
    }
    debug!("{} on slot {}: {} entries", command, slot, map.len());
    client
        .call_board(
            slot,
            &format!("Xger:{ver}/{module}/{command}"),
            json!({ "data": Value::Object(map) }),
        )
        .await
        .map_err(vendor_error)
}

/// Like `xger_set_keyed` but the payload is a single object — the API shape
/// is `{data: T}` (not a UUID-keyed map). Used by `set_pool_config` etc.
async fn xger_set_raw(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    action: &Value,
    payload_key: &str,
) -> Result<Value, CommandError> {
    let action_name = command_to_action(command);
    let ver = xger_version(state, slot, module, &action_name)?;
    let data = action.get(payload_key).cloned().ok_or_else(|| {
        CommandError::validation(format!("missing '{payload_key}' field").as_str())
    })?;
    client
        .call_board(
            slot,
            &format!("Xger:{ver}/{module}/{command}"),
            json!({ "data": data }),
        )
        .await
        .map_err(vendor_error)
}

/// Convert a vendor command verb (e.g. `GetCoderServices`, `SetAudioProfiles`)
/// into the snake_case action name an operator would type at the manager
/// (`get_coder_services`). Used purely to make the `unsupported_on_card`
/// message match what they sent.
fn command_to_action(command: &str) -> String {
    let mut out = String::with_capacity(command.len() + 4);
    for (i, ch) in command.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Phase 3a: per-encoder / per-decoder Get/Set router. Routes on the slot's
/// discovered family (`hipEnc` JPEG-XS vs `hipTsEnc` HEVC-TS for the encoder
/// side; `hipDec` vs `hipTsDec` for the decoder side). Mirrors the selector
/// logic used by `clear_all_counters`. Returns `unsupported_on_card` when no
/// encoder/decoder family is on the slot.
///
/// For Set, the operator's payload lives at `action.config` and is sent
/// pass-through as `{"data": <config>}` — schema varies by firmware.
async fn hip_call(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    encoder: bool,
    set: bool,
    action: &Value,
) -> Result<Value, CommandError> {
    let candidates: &[(&str, &str)] = if encoder {
        &[("hipTsEnc", "hipTsEncoder"), ("hipEnc", "hipEncoder")]
    } else {
        &[("hipTsDec", "hipTsDecoder"), ("hipDec", "hipDecoder")]
    };
    let mut chosen: Option<(&str, &str, String)> = None;
    for (iface, module) in candidates {
        if let Some(v) = state.discovered_version(slot, iface, module) {
            chosen = Some((iface, module, v));
            break;
        }
    }
    let (iface, module, ver) = chosen.ok_or_else(|| {
        CommandError::new(
            "unsupported_on_card",
            format!(
                "Slot {slot} exposes no {kind} module ({fam}). Commission an {kind} pool first.",
                kind = if encoder { "encoder" } else { "decoder" },
                fam = candidates.iter().map(|(i, _)| *i).collect::<Vec<_>>().join(" / "),
            ),
        )
    })?;
    let cmd = if set {
        if encoder {
            "SetEncoders"
        } else {
            "SetDecoders"
        }
    } else if encoder {
        "GetEncoders"
    } else {
        "GetDecoders"
    };
    let params = if set {
        let cfg = action.get("config").cloned().ok_or_else(|| {
            CommandError::validation("missing 'config' field for set command")
        })?;
        json!({ "data": cfg })
    } else {
        json!({})
    };
    client
        .call_board(slot, &format!("{iface}:{ver}/{module}/{cmd}"), params)
        .await
        .map_err(vendor_error)
}

/// Issue an Xger Delete command with an `ids: [UUID, …]` body.
async fn xger_delete(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    action: &Value,
) -> Result<Value, CommandError> {
    let action_name = command_to_action(command);
    let ver = xger_version(state, slot, module, &action_name)?;
    let ids = action
        .get("ids")
        .cloned()
        .ok_or_else(|| CommandError::validation("missing 'ids' field"))?;
    if !ids.is_array() {
        return Err(CommandError::validation("'ids' must be an array"));
    }
    client
        .call_board(
            slot,
            &format!("Xger:{ver}/{module}/{command}"),
            json!({ "ids": ids }),
        )
        .await
        .map_err(vendor_error)
}

fn require_field<'a>(action: &'a Value, key: &str) -> Result<&'a Value, CommandError> {
    action
        .get(key)
        .ok_or_else(|| CommandError::validation(format!("missing '{key}' field").as_str()))
}

/// Map a vendor-side `anyhow::Error` onto the SDK's `CommandError`. All
/// non-validation errors go through this helper so every failed vendor call
/// lands on `command_ack.error_code = "vendor_api_error"` — mirroring the
/// edge's convention of using a stable machine-readable error-code taxonomy.
fn vendor_error(e: anyhow::Error) -> CommandError {
    CommandError::new("vendor_api_error", format!("{e:#}"))
}
