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
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "bilbycast-appear-x-api-gateway")]
#[command(about = "Bridges Appear X platform to bilbycast-manager")]
struct Cli {
    /// Path to the TOML configuration file
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway (default).
    Run,
    /// Connect to the configured Appear X unit and exercise each polling
    /// JSON-RPC call once. Does not connect to the manager. Useful for
    /// verifying credentials and discovering which interface versions a
    /// particular firmware exposes.
    Probe,
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
    let cfg = config::AppConfig::load_for_command(&cli.config, matches!(cli.command, Some(Command::Probe)))?;

    if matches!(cli.command, Some(Command::Probe)) {
        return run_probe(&cfg).await;
    }

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

    // Discover the chassis type and per-slot card capabilities up front. The
    // result drives which per-slot polling tasks are spawned, so we never call
    // methods this firmware doesn't understand.
    info!("Running Appear X capability discovery…");
    let cards_mmi_versions = ["2.8", "2.16", "4.1", "1.0"];
    let caps = match appear_x::capabilities::discover(&appear_client, &cards_mmi_versions).await {
        Ok(c) => {
            info!("Capability discovery: {}", c.summary());
            c
        }
        Err(e) => {
            error!("Capability discovery failed: {e:#}");
            return Err(e);
        }
    };

    // Create channels for communication between polling and WS client
    let (stats_tx, stats_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<ws::message::CommandMessage>(32);

    // Shared state owned by both the polling engine (writers) and the
    // command handler (reader, for `get_config` responses).
    let shared_state = appear_x::state::SharedAppearXState::new(
        caps.clone(),
        env!("CARGO_PKG_VERSION"),
        cfg.appear_x.address.clone(),
    );

    // Spawn the polling engine
    let polling_cancel = cancel.child_token();
    let polling_client = appear_client.clone();
    let polling_cfg = cfg.polling.clone();
    let polling_caps = caps.clone();
    let polling_state = shared_state.clone();
    let polling_stats_tx = stats_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = appear_x::polling::run_polling(
            polling_client,
            polling_cfg,
            polling_caps,
            polling_state,
            polling_stats_tx,
            polling_cancel,
        )
        .await
        {
            error!("Polling engine error: {}", e);
        }
    });

    // Spawn the command handler. It also gets a stats_tx clone so it can
    // emit `config_response` envelopes when the manager issues `get_config`.
    let cmd_cancel = cancel.child_token();
    let cmd_client = appear_client.clone();
    let cmd_state = shared_state.clone();
    let cmd_stats_tx = stats_tx.clone();
    tokio::spawn(async move {
        appear_x::commands::run_command_handler(
            cmd_client,
            cmd_state,
            cmd_stats_tx,
            cmd_rx,
            cmd_cancel,
        )
        .await;
    });

    // Drop the original stats_tx — both polling and the command handler now
    // hold their own clones, so the channel will stay open until both tasks
    // finish. Without this, an extra reference would prevent stats_rx from
    // ever observing channel closure on shutdown.
    drop(stats_tx);

    // Run the WebSocket client (blocks until cancelled)
    ws::client::run_ws_client(cfg.manager.clone(), creds, stats_rx, cmd_tx, cancel).await?;

    info!("Gateway shut down cleanly");
    Ok(())
}

/// Connect to the configured Appear X unit and exercise each polling JSON-RPC
/// call once. Prints a one-line PASS/FAIL summary plus a truncated response body
/// per call so the user can verify which interface versions their firmware exposes.
async fn run_probe(cfg: &config::AppConfig) -> Result<()> {
    use appear_x::jsonrpc::JsonRpcClient;

    println!(
        "Probing Appear X unit at https://{} (user: {})",
        cfg.appear_x.address, cfg.appear_x.username
    );
    let client = JsonRpcClient::new(&cfg.appear_x)?;

    // Authentication is the prerequisite for everything else.
    match client.authenticate().await {
        Ok(()) => println!("  PASS  authenticate (mmi:1.0/authentication/BeginSession)"),
        Err(e) => {
            println!("  FAIL  authenticate: {e:#}");
            return Err(e);
        }
    }

    let p = &cfg.polling;
    let mmi_calls: Vec<(String, &'static str, serde_json::Value)> = vec![
        (
            format!("mmi:{}/alarms/GetActiveAlarms", p.alarms_mmi_version),
            "alarms",
            json!({"query": {}}),
        ),
        (
            format!("mmi:{}/chassisModel/GetGraph", p.chassis_mmi_version),
            "chassis",
            json!({}),
        ),
        (
            format!("mmi:{}/cards/GetChassisInfo", p.cards_mmi_version),
            "cards/GetChassisInfo",
            json!({}),
        ),
        (
            format!("mmi:{}/cards/GetCardStates", p.cards_mmi_version),
            "cards/GetCardStates",
            json!({}),
        ),
    ];

    for (method, label, params) in &mmi_calls {
        match client.call_mmi(method, params.clone()).await {
            Ok(v) => {
                let preview = serde_json::to_string(&v)
                    .map(|s| {
                        if s.len() > 200 {
                            format!("{}…", &s[..200])
                        } else {
                            s
                        }
                    })
                    .unwrap_or_default();
                println!("  PASS  {label:24} ({method}) -> {preview}");
            }
            Err(e) => {
                println!("  FAIL  {label:24} ({method}) -> {e}");
            }
        }
    }

    // Run the per-slot capability discovery against this firmware. The probe
    // walks the static `CARD_PROBES` registry trying every (interface, version,
    // module, command) candidate and records what responds. Most candidates
    // will return `Method not found` on any given card — that's how we narrow
    // the universe down to "the things this card software actually supports".
    println!();
    println!("Discovering per-slot card interfaces…");
    let cards_mmi_versions = ["2.8", "2.16", "4.1", "1.0"];
    let caps = match appear_x::capabilities::discover(&client, &cards_mmi_versions).await {
        Ok(c) => c,
        Err(e) => {
            println!("  FAIL  discovery: {e:#}");
            return Err(e);
        }
    };

    println!();
    println!("Discovered capabilities:");
    println!(
        "  Chassis: {} (MMI cards/* version: {})",
        caps.chassis_type, caps.cards_mmi_version
    );
    if caps.slots.is_empty() {
        println!("  (no card slots reported)");
    }
    for (slot, slot_caps) in &caps.slots {
        println!(
            "  Slot {slot}: {} sn={} sw={} ver={}",
            slot_caps.name,
            slot_caps.serial,
            slot_caps
                .software_id
                .clone()
                .unwrap_or_else(|| "?".into()),
            slot_caps
                .software_version
                .clone()
                .unwrap_or_else(|| "?".into()),
        );
        if !slot_caps.features.is_empty() {
            println!("         features: {}", slot_caps.features.join(", "));
        }
        if slot_caps.discovered_interfaces.is_empty() {
            println!(
                "         no card-level interfaces matched the probe registry — \
                 this firmware uses a namespace not yet registered in \
                 src/appear_x/probe_registry.rs"
            );
        } else {
            for (iface, rec) in &slot_caps.discovered_interfaces {
                println!(
                    "         ✓ {iface}:{} (family={}) via {}",
                    rec.version, rec.family, rec.probe_method
                );
            }
        }
    }

    println!();
    println!("Probe complete.");
    Ok(())
}
