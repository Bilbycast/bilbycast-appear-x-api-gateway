// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Command handler that translates manager commands into Appear X JSON-RPC calls.
//!
//! Implements the SDK's [`CommandHandler`] trait — the SDK owns the WS read
//! loop, the `command_ack` serialisation, and the `get_config` →
//! `config_response` envelope dance. This module is purely the vendor
//! translation layer: `action["type"]` → JSON-RPC method.

use async_trait::async_trait;
use bilbycast_gateway_sdk::{CommandError, CommandHandler};
use serde_json::{json, Value};
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
            "get_inputs" => {
                let api_version = get_api_version(&action, &self.state, slot, "ipGateway");
                let method = format!("ipGateway:{api_version}/input/GetInputs");
                self.client
                    .call_board(slot, &method, json!({}))
                    .await
                    .map_err(vendor_error)
            }

            "get_outputs" => {
                let api_version = get_api_version(&action, &self.state, slot, "ipGateway");
                let method = format!("ipGateway:{api_version}/output/GetOutputs");
                self.client
                    .call_board(slot, &method, json!({}))
                    .await
                    .map_err(vendor_error)
            }

            "get_services" => {
                let api_version = get_api_version(&action, &self.state, slot, "board");
                let method = format!("board:{api_version}/services/GetInputServices");
                self.client
                    .call_board(slot, &method, json!({"query": {}}))
                    .await
                    .map_err(vendor_error)
            }

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

            "get_ip_interfaces" => {
                let api_version = get_api_version(&action, &self.state, slot, "ipGateway");
                let method = format!("ipGateway:{api_version}/ipinterface/GetIpInterfaces");
                self.client
                    .call_board(slot, &method, json!({}))
                    .await
                    .map_err(vendor_error)
            }

            "set_ip_input" => {
                let api_version = get_api_version(&action, &self.state, slot, "ipGateway");
                let inputs = action.get("inputs").ok_or_else(|| {
                    CommandError::validation("set_ip_input requires 'inputs' field")
                })?;
                let method = format!("ipGateway:{api_version}/input/SetInputs");
                debug!(
                    "SetInputs on slot {}: {} inputs",
                    slot,
                    inputs.as_array().map(|a| a.len()).unwrap_or(0)
                );
                self.client
                    .call_board(slot, &method, json!({"data": inputs}))
                    .await
                    .map_err(vendor_error)
            }

            "set_ip_output" => {
                let api_version = get_api_version(&action, &self.state, slot, "ipGateway");
                let outputs = action.get("outputs").ok_or_else(|| {
                    CommandError::validation("set_ip_output requires 'outputs' field")
                })?;
                let method = format!("ipGateway:{api_version}/output/SetOutputs");
                debug!(
                    "SetOutputs on slot {}: {} outputs",
                    slot,
                    outputs.as_array().map(|a| a.len()).unwrap_or(0)
                );
                self.client
                    .call_board(slot, &method, json!({"data": outputs}))
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

/// Map a vendor-side `anyhow::Error` onto the SDK's `CommandError`. All
/// non-validation errors go through this helper so every failed vendor call
/// lands on `command_ack.error_code = "vendor_api_error"` — mirroring the
/// edge's convention of using a stable machine-readable error-code taxonomy.
fn vendor_error(e: anyhow::Error) -> CommandError {
    CommandError::new("vendor_api_error", format!("{e:#}"))
}

/// Extract API version from the action, falling back to the version
/// discovered at startup for this slot+interface, then to "1.15".
fn get_api_version(
    action: &Value,
    state: &SharedAppearXState,
    slot: u32,
    interface: &str,
) -> String {
    if let Some(v) = action.get("api_version").and_then(|v| v.as_str()) {
        return v.to_string();
    }
    if let Some(v) = state.discovered_version(slot, interface) {
        return v;
    }
    "1.15".to_string()
}
