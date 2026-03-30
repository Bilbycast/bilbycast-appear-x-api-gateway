// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! WebSocket client connecting to bilbycast-manager.
//! Implements the same auth protocol as bilbycast-edge and bilbycast-relay.

use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::ManagerConfig;
use crate::credentials::Credentials;
use crate::ws::message::{self, CommandMessage};
use crate::ws::tls;

/// Run the WebSocket client with automatic reconnection.
pub async fn run_ws_client(
    config: ManagerConfig,
    mut creds: Credentials,
    mut stats_rx: mpsc::Receiver<serde_json::Value>,
    cmd_tx: mpsc::Sender<CommandMessage>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut backoff_secs = 1u64;
    let max_backoff = 60u64;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        match try_connect(&config, &mut creds, &mut stats_rx, &cmd_tx, &cancel).await {
            Ok(ConnectResult::Registered) => {
                backoff_secs = 1;
            }
            Ok(ConnectResult::Closed) => {
                backoff_secs = 1;
            }
            Err(e) => {
                warn!("Manager connection failed: {}", e);
            }
        }

        if cancel.is_cancelled() {
            break;
        }

        info!("Reconnecting in {} seconds...", backoff_secs);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
            _ = cancel.cancelled() => break,
        }
        backoff_secs = (backoff_secs * 2).min(max_backoff);
    }

    Ok(())
}

enum ConnectResult {
    Registered,
    Closed,
}

async fn try_connect(
    config: &ManagerConfig,
    creds: &mut Credentials,
    stats_rx: &mut mpsc::Receiver<serde_json::Value>,
    cmd_tx: &mpsc::Sender<CommandMessage>,
    cancel: &CancellationToken,
) -> Result<ConnectResult> {
    info!("Connecting to manager at {}", config.url);

    // Build TLS config
    let tls_config = tls::build_tls_config(
        config.accept_self_signed_cert,
        config.cert_fingerprint.as_deref(),
    )?;

    let connector = tokio_tungstenite::Connector::Rustls(Arc::new(tls_config));
    let (ws_stream, _) =
        tokio_tungstenite::connect_async_tls_with_config(&config.url, None, false, Some(connector))
            .await?;

    info!("WebSocket connected, authenticating...");
    let (mut write, mut read) = ws_stream.split();

    // Send auth message
    let auth_msg = if creds.has_credentials() {
        message::auth_reconnect(
            creds.node_id.as_deref().unwrap(),
            creds.node_secret.as_deref().unwrap(),
        )
    } else if let Some(ref token) = creds.registration_token {
        message::auth_register(token)
    } else {
        bail!("No credentials or registration token available");
    };

    write.send(Message::Text(auth_msg.into())).await?;

    // Wait for auth response (10s timeout)
    let auth_response = tokio::time::timeout(Duration::from_secs(10), read.next())
        .await
        .map_err(|_| anyhow::anyhow!("Auth timeout — no response within 10 seconds"))?
        .ok_or_else(|| anyhow::anyhow!("Connection closed during auth"))??;

    let auth_text = auth_response
        .to_text()
        .map_err(|_| anyhow::anyhow!("Non-text auth response"))?;
    let auth_json: serde_json::Value = serde_json::from_str(auth_text)?;
    let msg_type = auth_json
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    let mut result = ConnectResult::Closed;

    match msg_type {
        "register_ack" => {
            let payload = auth_json.get("payload").unwrap_or(&serde_json::Value::Null);
            let node_id = payload
                .get("node_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("register_ack missing node_id"))?;
            let node_secret = payload
                .get("node_secret")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("register_ack missing node_secret"))?;

            info!("Registered with manager as node_id={}", node_id);
            creds.node_id = Some(node_id.to_string());
            creds.node_secret = Some(node_secret.to_string());
            creds.registration_token = None;
            creds.save(&config.credentials_file)?;
            result = ConnectResult::Registered;
        }
        "auth_ok" => {
            info!("Authenticated with manager");
        }
        "auth_error" => {
            let error = auth_json
                .get("payload")
                .and_then(|p| p.get("error"))
                .and_then(|e| e.as_str())
                .unwrap_or("unknown");
            bail!("Authentication failed: {}", error);
        }
        _ => {
            bail!("Unexpected auth response type: {}", msg_type);
        }
    }

    // Main message loop
    let mut health_interval = tokio::time::interval(Duration::from_secs(15));
    health_interval.tick().await; // skip first tick

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("Shutdown requested, closing WebSocket");
                break;
            }

            // Incoming message from manager
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_manager_message(&text, &mut write, cmd_tx).await;
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) => {
                        info!("Manager closed connection");
                        break;
                    }
                    Some(Err(e)) => {
                        error!("WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        info!("WebSocket stream ended");
                        break;
                    }
                    _ => {}
                }
            }

            // Outgoing stats/health from polling engine
            Some(stats) = stats_rx.recv() => {
                let msg_type = stats.get("_msg_type")
                    .and_then(|t: &serde_json::Value| t.as_str())
                    .unwrap_or("stats");
                let mut payload = stats.clone();
                if let Some(obj) = payload.as_object_mut() {
                    obj.remove("_msg_type");
                }

                let ws_msg = match msg_type {
                    "health" => message::health_message(payload),
                    _ => message::stats_message(payload),
                };
                if let Err(e) = write.send(Message::Text(ws_msg.into())).await {
                    error!("Failed to send stats: {}", e);
                    break;
                }
            }

            // Periodic health heartbeat
            _ = health_interval.tick() => {
                let health = serde_json::json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                });
                let ws_msg = message::health_message(health);
                if let Err(e) = write.send(Message::Text(ws_msg.into())).await {
                    error!("Failed to send health: {}", e);
                    break;
                }
            }
        }
    }

    Ok(result)
}

async fn handle_manager_message<S>(
    text: &str,
    write: &mut futures_util::stream::SplitSink<S, Message>,
    cmd_tx: &mpsc::Sender<CommandMessage>,
)
where
    S: futures_util::Sink<Message> + Unpin,
{
    let envelope: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            debug!("Failed to parse manager message: {}", e);
            return;
        }
    };

    let msg_type = envelope
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    match msg_type {
        "command" => {
            let payload = envelope.get("payload").cloned().unwrap_or_default();
            let command_id = payload
                .get("command_id")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let action = payload.get("action").cloned().unwrap_or_default();

            // Send command to handler, wait for ack
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let cmd = CommandMessage {
                command_id: command_id.clone(),
                action,
                ack_tx,
            };

            if cmd_tx.send(cmd).await.is_ok() {
                // Wait for the command handler to process and return ack
                match tokio::time::timeout(Duration::from_secs(10), ack_rx).await {
                    Ok(Ok(ack_data)) => {
                        let success = ack_data
                            .get("success")
                            .and_then(|s| s.as_bool())
                            .unwrap_or(false);
                        let error = ack_data
                            .get("error")
                            .and_then(|e| e.as_str())
                            .map(|s| s.to_string());
                        let data = ack_data.get("data").cloned();
                        let ack =
                            message::command_ack(&command_id, success, data, error.as_deref());
                        let _ = write.send(Message::Text(ack.into())).await;
                    }
                    _ => {
                        let ack = message::command_ack(
                            &command_id,
                            false,
                            None,
                            Some("Command handler timeout"),
                        );
                        let _ = write.send(Message::Text(ack.into())).await;
                    }
                }
            } else {
                let ack = message::command_ack(
                    &command_id,
                    false,
                    None,
                    Some("Command handler not available"),
                );
                let _ = write.send(Message::Text(ack.into())).await;
            }
        }
        "ping" => {
            let pong = message::pong_message();
            let _ = write.send(Message::Text(pong.into())).await;
        }
        _ => {
            debug!("Unknown manager message type: {}", msg_type);
        }
    }
}
