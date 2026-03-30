// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! bilbycast-appear-x-api-gateway — bridges Appear X JSON-RPC API to bilbycast-manager.
//!
//! This gateway connects to bilbycast-manager as a WebSocket client (same protocol
//! as edge/relay nodes) and polls an Appear X unit via its JSON-RPC 2.0 API,
//! translating stats/health/commands between the two systems.

mod appear_x;
mod config;
mod credentials;
mod ws;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "bilbycast-appear-x-api-gateway")]
#[command(about = "Bridges Appear X platform to bilbycast-manager")]
struct Cli {
    /// Path to the TOML configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,bilbycast_appear_x_api_gateway=debug".into()),
        )
        .init();

    let cli = Cli::parse();

    info!("Loading configuration from {:?}", cli.config);
    let cfg = config::AppConfig::load(&cli.config)?;

    let cancel = CancellationToken::new();

    // Set up ctrl-c handler
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Received shutdown signal");
        cancel_clone.cancel();
    });

    // Load or prepare credentials
    let creds = credentials::Credentials::load_or_default(&cfg.manager)?;

    // Build the Appear X JSON-RPC client
    let appear_client = appear_x::jsonrpc::JsonRpcClient::new(&cfg.appear_x)?;

    // Create channels for communication between polling and WS client
    let (stats_tx, stats_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<ws::message::CommandMessage>(32);

    // Spawn the polling engine
    let polling_cancel = cancel.child_token();
    let polling_client = appear_client.clone();
    let polling_cfg = cfg.polling.clone();
    tokio::spawn(async move {
        if let Err(e) = appear_x::polling::run_polling(
            polling_client,
            polling_cfg,
            stats_tx,
            polling_cancel,
        )
        .await
        {
            error!("Polling engine error: {}", e);
        }
    });

    // Spawn the command handler
    let cmd_cancel = cancel.child_token();
    let cmd_client = appear_client.clone();
    tokio::spawn(async move {
        appear_x::commands::run_command_handler(cmd_client, cmd_rx, cmd_cancel).await;
    });

    // Run the WebSocket client (blocks until cancelled)
    ws::client::run_ws_client(cfg.manager.clone(), creds, stats_rx, cmd_tx, cancel).await?;

    info!("Gateway shut down cleanly");
    Ok(())
}
