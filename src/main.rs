// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! bilbycast-appear-x-api-gateway — bridges Appear X JSON-RPC API to bilbycast-manager.
//!
//! This gateway connects to bilbycast-manager as a WebSocket client (same protocol
//! as edge/relay nodes) and polls an Appear X unit via its JSON-RPC 2.0 API,
//! translating stats/health/commands between the two systems.
//!
//! The manager-facing WS plumbing (auth, reconnect, heartbeat, TLS, envelope
//! serialisation) lives in [`bilbycast_gateway_sdk`]; this crate is the
//! Appear X vendor translation layer on top of it.

mod appear_x;
mod config;
mod event_gate;

use anyhow::{Context, Result};
use bilbycast_gateway_sdk::{
    CredentialStore, GatewayClient, GatewayConfig, PersistedCredentials,
};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
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
    let cfg = config::AppConfig::load_for_command(
        &cli.config,
        matches!(cli.command, Some(Command::Probe)),
    )?;

    if matches!(cli.command, Some(Command::Probe)) {
        return run_probe(&cfg).await;
    }

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

    // Shared state owned by both the polling engine (writers) and the
    // command handler (reader, for `get_config` responses).
    let shared_state = appear_x::state::SharedAppearXState::new(
        caps.clone(),
        env!("CARGO_PKG_VERSION"),
        cfg.appear_x.address.clone(),
    );

    // Build the SDK gateway client, seeded from persisted credentials when
    // available. `CredentialStore` handles the 0600 JSON blob for us.
    let credentials_file = cfg.manager.credentials_file.clone();
    let store = CredentialStore::new(credentials_file.clone());
    let persisted = store.load().with_context(|| {
        format!("Failed to load credentials from {credentials_file}")
    })?;

    let mut gateway_cfg = GatewayConfig {
        manager_urls: cfg.manager.urls.clone(),
        device_type: "appear_x".into(),
        software_version: env!("CARGO_PKG_VERSION").into(),
        node_id: persisted.node_id.clone(),
        node_secret: persisted.node_secret.clone(),
        registration_token: None,
        accept_self_signed_cert: cfg.manager.accept_self_signed_cert,
        cert_fingerprint: cfg.manager.cert_fingerprint.clone(),
        heartbeat_interval: std::time::Duration::from_secs(15),
        reconnect_backoff: Default::default(),
    };
    if !persisted.has_credentials() {
        gateway_cfg.registration_token = cfg.manager.registration_token.clone();
    }

    let handler = Arc::new(appear_x::commands::AppearXCommandHandler::new(
        appear_client.clone(),
        shared_state.clone(),
    ));

    let mut client = GatewayClient::connect(gateway_cfg, handler).await?;
    let shutdown = client.shutdown_token();

    // Persist credentials after first-time registration.
    let store_cb = store.clone();
    client.on_register(move |node_id, node_secret| {
        let creds = PersistedCredentials {
            node_id: Some(node_id.to_string()),
            node_secret: Some(node_secret.to_string()),
            registration_token: None,
        };
        if let Err(e) = store_cb.save(&creds) {
            error!("Failed to persist credentials: {e}");
        } else {
            info!("Credentials persisted after registration");
        }
    });

    // Wire ctrl-c → SDK shutdown token so the connect loop exits cleanly.
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("Received shutdown signal");
            shutdown.cancel();
        });
    }

    // Spawn the polling engine. It emits stats/health/events directly
    // through the SDK's Emitter (which feeds the WS write task).
    let polling_emitter = client.emitter();
    let polling_cancel = shutdown.clone();
    let polling_client = appear_client.clone();
    let polling_cfg = cfg.polling.clone();
    let polling_caps = caps.clone();
    let polling_state = shared_state.clone();
    tokio::spawn(async move {
        if let Err(e) = appear_x::polling::run_polling(
            polling_client,
            polling_cfg,
            polling_caps,
            polling_state,
            polling_emitter,
            polling_cancel,
        )
        .await
        {
            error!("Polling engine error: {e}");
        }
    });

    // Run the WS client (blocks until the shutdown token fires).
    client.run().await?;

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
        if slot_caps.discovered_modules.is_empty() {
            println!(
                "         no card-level modules matched the probe registry — \
                 this firmware uses a namespace not yet registered in \
                 src/appear_x/probe_registry.rs"
            );
        } else {
            for (key, rec) in &slot_caps.discovered_modules {
                println!(
                    "         ✓ {}:{}  (family={})  via {}",
                    key, rec.version, rec.family, rec.probe_method
                );
            }
        }
    }

    // Extra: hit each discovered Xger module once and surface the broadcast-
    // critical signals broadcast engineers care about (PTP lock, SFP RX
    // power, SFP temperature). Runs independently per slot so one dead
    // module doesn't kill the whole report.
    println!();
    println!("Xger health snapshot (one-shot per slot):");
    for (slot, slot_caps) in &caps.slots {
        if slot_caps.discovered_modules.is_empty() {
            continue;
        }
        println!("  Slot {slot}:");
        let ver = slot_caps
            .any_interface_version("Xger")
            .unwrap_or("2.55");
        // Card status — PTP + SFP
        let cs_method = format!("Xger:{ver}/cardStatus/GetCardStatus");
        match client
            .call_board(*slot, &cs_method, json!({"slot": slot}))
            .await
        {
            Ok(v) => print_card_status_summary(&v),
            Err(e) => println!("    cardStatus failed: {e}"),
        }
        // One-liner for each other probed module
        for (key, rec) in &slot_caps.discovered_modules {
            if key == "Xger/cardStatus" {
                continue;
            }
            let (module, command) = match key.split_once('/') {
                Some((_, m)) => match m {
                    "cardAllocation" => (m, "GetCardAllocations"),
                    "multiService" => (m, "GetMultiServices"),
                    "audioProfile" => (m, "GetAudioProfiles"),
                    "ipInterface" => (m, "GetIpInterfaces"),
                    "imageUpload" => (m, "GetImages"),
                    "poolConfig" => (m, "GetPoolConfig"),
                    "coderService" => (m, "GetCoderServices"),
                    "ipConnection" => (m, "GetIpConnections"),
                    "lockStatus" => (m, "GetLockStatus"),
                    "psiStatus" => (m, "GetPsiStatus"),
                    _ => continue,
                },
                None => continue,
            };
            let method = format!("Xger:{}/{}/{}", rec.version, module, command);
            match client.call_board(*slot, &method, json!({})).await {
                Ok(v) => {
                    let count = v
                        .get("data")
                        .and_then(|d| d.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let size = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
                    println!(
                        "    {:20} OK  (data.len={count}, response bytes={size})",
                        module
                    );
                }
                Err(e) => println!("    {:20} ERR {}", module, e),
            }
        }
    }

    println!();
    println!("Probe complete.");
    Ok(())
}

/// Format a `cardStatus` result for the human probe report — pulls out the
/// three things broadcast engineers actually ask about: PTP lock, worst SFP
/// RX optical power, max SFP temperature.
fn print_card_status_summary(status: &serde_json::Value) {
    // PTP
    let ptp = status
        .get("ptpLock")
        .and_then(|pl| {
            if pl.is_object() && pl.as_object().map(|o| o.is_empty()).unwrap_or(false) {
                Some("no-signal".to_string())
            } else {
                pl.get("state")
                    .or_else(|| pl.pointer("/value/state"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            }
        })
        .unwrap_or_else(|| "unknown".into());
    println!("    cardStatus          OK  ptp={ptp}");

    let mut rx_min: Option<f64> = None;
    let mut temp_max: Option<f64> = None;
    let qsfp = status.pointer("/qsfpStatus/value").and_then(|v| v.as_array());
    let sfp = status.pointer("/sfpStatus/value").and_then(|v| v.as_array());
    for arr in [qsfp, sfp].into_iter().flatten() {
        for entry in arr {
            let port = entry
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let diag = entry.pointer("/value/diagnostics/value");
            if let Some(d) = diag {
                let temp = d.get("temp").and_then(|v| v.as_f64());
                let rx: Vec<f64> = d
                    .get("rxPwr")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
                    .unwrap_or_default();
                let rx_dbm: Vec<String> = rx
                    .iter()
                    .map(|mw| {
                        if *mw > 0.0 {
                            format!("{:.1}dBm", 10.0 * mw.log10())
                        } else {
                            "--".to_string()
                        }
                    })
                    .collect();
                if let Some(t) = temp {
                    temp_max = Some(match temp_max {
                        Some(v) => v.max(t),
                        None => t,
                    });
                }
                for mw in &rx {
                    if *mw > 0.0 {
                        let dbm = 10.0 * mw.log10();
                        rx_min = Some(match rx_min {
                            Some(v) => v.min(dbm),
                            None => dbm,
                        });
                    }
                }
                println!(
                    "      port {port}: temp={}  rx=[{}]",
                    temp.map(|t| format!("{t:.1}°C")).unwrap_or_else(|| "?".into()),
                    rx_dbm.join(", ")
                );
            }
        }
    }
    let rx_s = rx_min
        .map(|v| format!("{v:.1} dBm"))
        .unwrap_or_else(|| "no-signal".into());
    let temp_s = temp_max
        .map(|v| format!("{v:.1} °C"))
        .unwrap_or_else(|| "-".into());
    println!("    → worst_rx={rx_s}  max_temp={temp_s}");
}
