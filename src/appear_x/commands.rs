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
use bilbycast_gateway_sdk::{CommandError, CommandHandler};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use super::jsonrpc::JsonRpcClient;
use super::state::SharedAppearXState;

/// Vendor-side command handler, wired up to the SDK via [`CommandHandler`].
pub struct AppearXCommandHandler {
    client: JsonRpcClient,
    state: SharedAppearXState,
}

impl AppearXCommandHandler {
    pub fn new(client: JsonRpcClient, state: SharedAppearXState) -> Self {
        Self { client, state }
    }
}

/// Wrapper handler used during the startup window where the manager WS is up
/// but Appear X capability discovery hasn't completed yet (chassis powered
/// down, on the wrong subnet, etc.). The sidecar registers this immediately
/// so the manager can render the node as Online with `gateway_target.reachable
/// = false`; once discovery succeeds the real [`AppearXCommandHandler`] is
/// installed and every command thereafter forwards to it.
///
/// While the inner handler is `None`, every command returns a
/// `discovery_in_progress` `CommandError` so the manager UI can show a
/// "chassis unreachable" banner instead of a generic timeout.
pub struct DeferredAppearXHandler {
    inner: RwLock<Option<Arc<AppearXCommandHandler>>>,
}

impl DeferredAppearXHandler {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(None),
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

impl Default for DeferredAppearXHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CommandHandler for DeferredAppearXHandler {
    async fn handle_command(
        &self,
        command_id: String,
        action: Value,
    ) -> Result<Value, CommandError> {
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
            "get_alarms" => self
                .client
                .call_mmi(
                    "mmi:2.16/alarms/GetActiveAlarms",
                    json!({"query": {}}),
                )
                .await
                .map_err(vendor_error),
            "get_chassis" => self
                .client
                .call_mmi("mmi:2.16/chassisModel/GetGraph", json!({}))
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

            // Xger write commands — all Get/Set symmetrical. Payload key
            // mirrors the Appear API (`data: [...]`).
            "set_coder_services" => xger_set(
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
            "set_multi_services" => xger_set(
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
            "set_audio_profiles" => xger_set(
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
            "set_xger_ip_interfaces" => xger_set(
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
            "set_card_allocations" => xger_set(
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

/// Resolve the Xger interface version for a specific module. Falls back to
/// any discovered Xger module's version, then "2.55".
fn xger_version(state: &SharedAppearXState, slot: u32, module: &str) -> String {
    if let Some(v) = state.discovered_version(slot, "Xger", module) {
        return v;
    }
    if let Some(v) = state.any_interface_version(slot, "Xger") {
        return v;
    }
    "2.55".to_string()
}

/// Issue a plain Xger Get call and return the raw JSON-RPC result.
async fn xger_call(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    params: Value,
) -> Result<Value, CommandError> {
    let ver = xger_version(state, slot, module);
    client
        .call_board(slot, &format!("Xger:{ver}/{module}/{command}"), params)
        .await
        .map_err(vendor_error)
}

/// Issue an Xger Set command with a `data: [...]` body. `payload_key` is the
/// action field where the operator places the array (e.g. `coder_services`).
async fn xger_set(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    action: &Value,
    payload_key: &str,
) -> Result<Value, CommandError> {
    let ver = xger_version(state, slot, module);
    let data = action.get(payload_key).cloned().ok_or_else(|| {
        CommandError::validation(format!("missing '{payload_key}' field").as_str())
    })?;
    debug!(
        "{} on slot {}: {} items",
        command,
        slot,
        data.as_array().map(|a| a.len()).unwrap_or(0)
    );
    client
        .call_board(
            slot,
            &format!("Xger:{ver}/{module}/{command}"),
            json!({ "data": data }),
        )
        .await
        .map_err(vendor_error)
}

/// Like `xger_set` but the payload is a single object, not an array.
async fn xger_set_raw(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    action: &Value,
    payload_key: &str,
) -> Result<Value, CommandError> {
    let ver = xger_version(state, slot, module);
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

/// Issue an Xger Delete command with an `ids: [UUID, …]` body.
async fn xger_delete(
    client: &JsonRpcClient,
    state: &SharedAppearXState,
    slot: u32,
    module: &str,
    command: &str,
    action: &Value,
) -> Result<Value, CommandError> {
    let ver = xger_version(state, slot, module);
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
