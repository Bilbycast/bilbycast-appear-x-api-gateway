// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! WebSocket message builders matching the bilbycast manager protocol.

use chrono::Utc;
use serde_json::json;

/// A command received from the manager that needs to be executed on the device.
#[derive(Debug)]
pub struct CommandMessage {
    pub command_id: String,
    pub action: serde_json::Value,
    /// Channel to send the ack back through
    pub ack_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
}

/// Build a WsEnvelope for the manager protocol.
pub fn ws_envelope(msg_type: &str, payload: serde_json::Value) -> String {
    let envelope = json!({
        "type": msg_type,
        "timestamp": Utc::now().to_rfc3339(),
        "payload": payload,
    });
    serde_json::to_string(&envelope).unwrap_or_default()
}

/// Build an auth message for first-time registration.
pub fn auth_register(registration_token: &str) -> String {
    ws_envelope(
        "auth",
        json!({
            "registration_token": registration_token,
            "software_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": 1,
        }),
    )
}

/// Build an auth message for reconnection.
pub fn auth_reconnect(node_id: &str, node_secret: &str) -> String {
    ws_envelope(
        "auth",
        json!({
            "node_id": node_id,
            "node_secret": node_secret,
            "software_version": env!("CARGO_PKG_VERSION"),
            "protocol_version": 1,
        }),
    )
}

/// Build a stats message.
pub fn stats_message(payload: serde_json::Value) -> String {
    ws_envelope("stats", payload)
}

/// Build a health message.
pub fn health_message(payload: serde_json::Value) -> String {
    ws_envelope("health", payload)
}

/// Build a config_response message. The manager populates its `cached_config`
/// only when this message type arrives — `command_ack.data` from a `get_config`
/// command is NOT used for that purpose. The manager stores `envelope.payload`
/// directly as `cached_config` and the API returns it as-is, so the payload
/// must be the bare config object (no wrapping).
pub fn config_response_message(config: serde_json::Value) -> String {
    ws_envelope("config_response", config)
}

/// Build a command acknowledgment.
pub fn command_ack(command_id: &str, success: bool, data: Option<serde_json::Value>, error: Option<&str>) -> String {
    let mut payload = json!({
        "command_id": command_id,
        "success": success,
    });
    if let Some(d) = data {
        payload["data"] = d;
    }
    if let Some(e) = error {
        payload["error"] = json!(e);
    }
    ws_envelope("command_ack", payload)
}

/// Build an event message for the manager's Events system.
pub fn event_message(payload: serde_json::Value) -> String {
    ws_envelope("event", payload)
}

/// Build a pong response.
pub fn pong_message() -> String {
    ws_envelope("pong", json!({}))
}
