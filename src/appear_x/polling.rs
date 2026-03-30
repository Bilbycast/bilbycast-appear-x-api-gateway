// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Polling engine that periodically fetches data from the Appear X unit
//! and sends it as stats/health messages to the manager via the WS client.

use anyhow::Result;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use super::jsonrpc::JsonRpcClient;
use crate::config::PollingConfig;

/// Run the polling engine — spawns tasks for each poll type.
pub async fn run_polling(
    client: JsonRpcClient,
    config: PollingConfig,
    stats_tx: mpsc::Sender<serde_json::Value>,
    cancel: CancellationToken,
) -> Result<()> {
    // Authenticate once at startup
    client.authenticate().await?;

    // Spawn alarm polling (MMI endpoint)
    spawn_poll(
        client.clone(),
        stats_tx.clone(),
        cancel.child_token(),
        config.alarms_interval_secs,
        "alarms",
        move |c| {
            Box::pin(async move {
                let result = c
                    .call_mmi(
                        "mmi:2.16/alarms/GetActiveAlarms",
                        json!({"query": {}}),
                    )
                    .await?;
                let alarms = result.get("data").cloned().unwrap_or(json!([]));

                // Derive health status from alarms
                let alarm_list = alarms.as_array();
                let has_major = alarm_list
                    .map(|a| {
                        a.iter().any(|alarm| {
                            alarm
                                .get("severity")
                                .and_then(|s| s.as_str())
                                .map(|s| s == "MAJOR" || s == "CRITICAL")
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                let has_minor = alarm_list
                    .map(|a| {
                        a.iter().any(|alarm| {
                            alarm
                                .get("severity")
                                .and_then(|s| s.as_str())
                                .map(|s| s == "MINOR" || s == "WARNING")
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);

                let status = if has_major {
                    "critical"
                } else if has_minor {
                    "degraded"
                } else {
                    "ok"
                };

                Ok(json!({
                    "_msg_type": "health",
                    "status": status,
                    "alarms": alarms,
                    "version": env!("CARGO_PKG_VERSION"),
                }))
            })
        },
    );

    // Spawn chassis polling (MMI endpoint)
    spawn_poll(
        client.clone(),
        stats_tx.clone(),
        cancel.child_token(),
        config.chassis_interval_secs,
        "chassis",
        move |c| {
            Box::pin(async move {
                let result = c.call_mmi("mmi:2.16/chassisModel/GetGraph", json!({})).await?;
                Ok(json!({
                    "chassis": result,
                }))
            })
        },
    );

    // Spawn per-board polling
    for board in &config.boards {
        let slot = board.slot;
        let api_version = board.api_version.clone();

        // Input polling
        let av = api_version.clone();
        spawn_poll(
            client.clone(),
            stats_tx.clone(),
            cancel.child_token(),
            config.inputs_interval_secs,
            &format!("inputs-slot{}", slot),
            move |c| {
                let av = av.clone();
                Box::pin(async move {
                    let method = format!("ipGateway:{}/input/GetInputs", av);
                    let result = c.call_board(slot, &method, json!({})).await?;
                    Ok(json!({
                        "inputs": result.get("data").cloned().unwrap_or(json!([])),
                        "slot": slot,
                    }))
                })
            },
        );

        // Output polling
        let av = api_version.clone();
        spawn_poll(
            client.clone(),
            stats_tx.clone(),
            cancel.child_token(),
            config.outputs_interval_secs,
            &format!("outputs-slot{}", slot),
            move |c| {
                let av = av.clone();
                Box::pin(async move {
                    let method = format!("ipGateway:{}/output/GetOutputs", av);
                    let result = c.call_board(slot, &method, json!({})).await?;
                    Ok(json!({
                        "outputs": result.get("data").cloned().unwrap_or(json!([])),
                        "slot": slot,
                    }))
                })
            },
        );

        // Services polling
        let av = api_version.clone();
        spawn_poll(
            client.clone(),
            stats_tx.clone(),
            cancel.child_token(),
            config.services_interval_secs,
            &format!("services-slot{}", slot),
            move |c| {
                let av = av.clone();
                Box::pin(async move {
                    let method = format!("{}:{}/services/GetInputServices", "board", av);
                    let result = c
                        .call_board(slot, &method, json!({"query": {}}))
                        .await?;
                    Ok(json!({
                        "services": result.get("data").cloned().unwrap_or(json!([])),
                        "slot": slot,
                    }))
                })
            },
        );

        // IP interfaces polling
        let av = api_version.clone();
        spawn_poll(
            client.clone(),
            stats_tx.clone(),
            cancel.child_token(),
            config.inputs_interval_secs * 2, // less frequent
            &format!("interfaces-slot{}", slot),
            move |c| {
                let av = av.clone();
                Box::pin(async move {
                    let method = format!("ipGateway:{}/ipinterface/GetIpInterfaces", av);
                    let result = c.call_board(slot, &method, json!({})).await?;
                    Ok(json!({
                        "ip_interfaces": result.get("data").cloned().unwrap_or(json!([])),
                        "slot": slot,
                    }))
                })
            },
        );
    }

    // Wait for cancellation
    cancel.cancelled().await;
    Ok(())
}

fn spawn_poll<F>(
    client: JsonRpcClient,
    tx: mpsc::Sender<serde_json::Value>,
    cancel: CancellationToken,
    interval_secs: u64,
    name: &str,
    poll_fn: F,
) where
    F: Fn(JsonRpcClient) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send>>
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
                    match poll_fn(client.clone()).await {
                        Ok(data) => {
                            debug!("Poll {} succeeded", name);
                            if tx.send(data).await.is_err() {
                                break; // channel closed
                            }
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
