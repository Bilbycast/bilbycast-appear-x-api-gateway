// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Command handler that translates manager commands into Appear X JSON-RPC calls.

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use super::jsonrpc::JsonRpcClient;
use crate::ws::message::CommandMessage;

/// Run the command handler loop — receives commands from the WS client
/// and translates them into Appear X API calls.
pub async fn run_command_handler(
    client: JsonRpcClient,
    mut cmd_rx: mpsc::Receiver<CommandMessage>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(cmd) = cmd_rx.recv() => {
                let result = handle_command(&client, &cmd.action).await;
                let ack = match result {
                    Ok(data) => json!({
                        "success": true,
                        "data": data,
                    }),
                    Err(e) => {
                        error!("Command {} failed: {}", cmd.command_id, e);
                        json!({
                            "success": false,
                            "error": e.to_string(),
                        })
                    }
                };
                let _ = cmd.ack_tx.send(ack);
            }
        }
    }
}

async fn handle_command(
    client: &JsonRpcClient,
    action: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let action_type = action
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");

    let slot = action.get("slot").and_then(|s| s.as_u64()).unwrap_or(1) as u32;

    info!("Handling command: {} (slot {})", action_type, slot);

    match action_type {
        "get_inputs" => {
            let api_version = get_api_version(action);
            let method = format!("ipGateway:{}/input/GetInputs", api_version);
            client.call_board(slot, &method, json!({})).await
        }

        "get_outputs" => {
            let api_version = get_api_version(action);
            let method = format!("ipGateway:{}/output/GetOutputs", api_version);
            client.call_board(slot, &method, json!({})).await
        }

        "get_services" => {
            let api_version = get_api_version(action);
            let method = format!("board:{}/services/GetInputServices", api_version);
            client.call_board(slot, &method, json!({"query": {}})).await
        }

        "get_alarms" => {
            client
                .call_mmi(
                    "mmi:2.16/alarms/GetActiveAlarms",
                    json!({"query": {}}),
                )
                .await
        }

        "get_chassis" => {
            client
                .call_mmi("mmi:2.16/chassisModel/GetGraph", json!({}))
                .await
        }

        "get_ip_interfaces" => {
            let api_version = get_api_version(action);
            let method = format!("ipGateway:{}/ipinterface/GetIpInterfaces", api_version);
            client.call_board(slot, &method, json!({})).await
        }

        "set_ip_input" => {
            let api_version = get_api_version(action);
            let inputs = action
                .get("inputs")
                .ok_or_else(|| anyhow::anyhow!("set_ip_input requires 'inputs' field"))?;
            let method = format!("ipGateway:{}/input/SetInputs", api_version);
            debug!("SetInputs on slot {}: {} inputs", slot, inputs.as_array().map(|a| a.len()).unwrap_or(0));
            client
                .call_board(slot, &method, json!({"data": inputs}))
                .await
        }

        "set_ip_output" => {
            let api_version = get_api_version(action);
            let outputs = action
                .get("outputs")
                .ok_or_else(|| anyhow::anyhow!("set_ip_output requires 'outputs' field"))?;
            let method = format!("ipGateway:{}/output/SetOutputs", api_version);
            debug!("SetOutputs on slot {}: {} outputs", slot, outputs.as_array().map(|a| a.len()).unwrap_or(0));
            client
                .call_board(slot, &method, json!({"data": outputs}))
                .await
        }

        _ => {
            anyhow::bail!("Unknown command type: {}", action_type);
        }
    }
}

/// Extract API version from the action, defaulting to "1.15".
fn get_api_version(action: &serde_json::Value) -> String {
    action
        .get("api_version")
        .and_then(|v| v.as_str())
        .unwrap_or("1.15")
        .to_string()
}
