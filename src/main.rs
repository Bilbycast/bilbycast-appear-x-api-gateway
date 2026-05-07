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
mod upgrade_profile;

use anyhow::{Context, Result};
use bilbycast_gateway_sdk::upgrade::{
    self, run_boot_watchdog, UpgradeCoordinator, UpgradeEvent, WatchdogOutcome,
};
use bilbycast_gateway_sdk::{
    CredentialStore, Emitter, EventSeverity, GatewayClient, GatewayConfig, GatewayEvent,
    PersistedCredentials,
};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

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

    // Build the SDK gateway client first, before discovery. Connecting to
    // the manager up front means an operator standing up a gateway against
    // a powered-down Appear X still sees their sidecar appear on the
    // dashboard — with `gateway_target.reachable = false` so the new
    // two-dot card renders "Sidecar Online / Target Unreachable" instead
    // of "Sidecar Offline / Target Unknown". The previous startup ordering
    // (discover-then-connect) made this state unreachable: the sidecar would
    // sit in its discovery retry loop forever and never say hello.
    //
    // The wire-level handler we register is a [`DeferredAppearXHandler`]
    // wrapper that returns a `discovery_in_progress` `command_ack.error_code`
    // for any command that arrives before discovery completes; once
    // discovery succeeds we install the real `AppearXCommandHandler` and
    // every subsequent command forwards normally.
    let credentials_file = cfg.manager.credentials_file.clone();
    let store = CredentialStore::new(credentials_file.clone());
    let persisted = store.load().with_context(|| {
        format!("Failed to load credentials from {credentials_file}")
    })?;

    // Remote-upgrade event channel — wired to the SDK's UpgradeCoordinator
    // and the boot watchdog. The receiver feeds an event-forwarder task
    // (spawned below) that translates each `UpgradeEvent` into a
    // `GatewayEvent` on the SDK Emitter, so upgrade lifecycle events
    // ride the same WS event path as every other vendor event the
    // sidecar emits.
    //
    // Channel exists even when `[upgrade]` is unset so the boot watchdog
    // and (eventually) the forwarder don't need conditional plumbing.
    // When upgrades are unconfigured the channel sits idle.
    let (upgrade_event_tx, upgrade_event_rx) =
        tokio::sync::mpsc::channel::<UpgradeEvent>(64);

    // Boot watchdog. Runs *before* we connect to the manager so a
    // crash-loop on a freshly-staged binary triggers the symlink
    // revert + `exit(1)` on the (max_boot_attempts + 1)th boot. The
    // queued `upgrade_rolled_back` Critical event drains over the WS
    // once the forwarder is up. No-op when `[upgrade]` is unset or
    // `enabled = false`.
    match run_boot_watchdog(cfg.upgrade.as_ref(), &upgrade_event_tx) {
        Ok(WatchdogOutcome::Continue) => {}
        Ok(WatchdogOutcome::PendingHealth { attempt }) => {
            info!(
                "Upgrade boot watchdog: this is boot attempt {attempt} on the staged version; \
                 will be promoted to stable after the configured health window."
            );
        }
        Ok(WatchdogOutcome::RolledBack { from_version, to_version }) => {
            warn!(
                "Upgrade boot watchdog: rolled back from {from_version} to {to_version} on \
                 the previous boot — `upgrade_rolled_back` will surface to the manager on auth."
            );
        }
        Err(e) => warn!("upgrade boot watchdog error: {e:#}"),
    }

    // Process-wide upgrade coordinator. `None` when `[upgrade]` is unset;
    // the `upgrade_binary` command then returns `upgrade_disabled` and
    // the `"upgrade"` capability is not advertised. The same `Arc` is
    // shared between the deferred handler (sidecar self-upgrade pre-
    // discovery) and the real handler (post-discovery).
    let upgrade_coord: Option<Arc<UpgradeCoordinator>> = cfg.upgrade.as_ref().map(|up_cfg| {
        Arc::new(UpgradeCoordinator::new(
            upgrade_profile::PROFILE,
            up_cfg.clone(),
            upgrade_event_tx.clone(),
            env!("CARGO_PKG_VERSION").to_string(),
        ))
    });
    if upgrade_coord.is_some() {
        info!("upgrade coordinator installed (profile: {})", upgrade_profile::PROFILE.repo);
    }

    // Long SDK heartbeat (5 min). The SDK's default `{status:"ok"}` heartbeat
    // would otherwise overwrite our rich `gateway_target`-bearing heartbeat
    // in the manager's `cached_health` every 15 s, and the dashboard would
    // flicker between Target Unreachable and Target Unknown. Application-
    // layer heartbeats (this file's discovery loop, then the polling engine)
    // run on a tighter cadence and carry the right shape — so the SDK
    // default is redundant, just suppressed by setting it large.
    let mut gateway_cfg = GatewayConfig {
        manager_urls: cfg.manager.urls.clone(),
        device_type: "appear_x".into(),
        software_version: env!("CARGO_PKG_VERSION").into(),
        node_id: persisted.node_id.clone(),
        node_secret: persisted.node_secret.clone(),
        registration_token: None,
        accept_self_signed_cert: cfg.manager.accept_self_signed_cert,
        cert_fingerprint: cfg.manager.cert_fingerprint.clone(),
        heartbeat_interval: std::time::Duration::from_secs(300),
        reconnect_backoff: Default::default(),
    };
    if !persisted.has_credentials() {
        gateway_cfg.registration_token = cfg.manager.registration_token.clone();
    }

    let deferred_handler = Arc::new(appear_x::commands::DeferredAppearXHandler::new(
        upgrade_coord.clone(),
    ));
    let mut client = GatewayClient::connect(gateway_cfg, deferred_handler.clone()).await?;

    let shutdown = client.shutdown_token();
    let emitter = client.emitter();

    // Drain queued upgrade events to the manager via the SDK Emitter.
    // Started after `client.emitter()` is available so any boot-watchdog
    // events that arrived synchronously above flush on the first WS
    // beat. Lives for the process lifetime; cancelled cleanly via the
    // shared shutdown token.
    spawn_upgrade_event_forwarder(upgrade_event_rx, emitter.clone(), shutdown.clone());

    // Periodic watchdog: promotes `PendingHealth` → `Stable` after the
    // configured boot-health window, emitting `upgrade_completed` once.
    // Only spawned when upgrades are enabled — for `enabled = false`
    // operators the state machine never enters `PendingHealth`.
    if let Some(ref up_cfg) = cfg.upgrade {
        if up_cfg.enabled {
            let install_root = up_cfg.install_root.clone();
            let cfg_clone = up_cfg.clone();
            let tx = upgrade_event_tx.clone();
            let cancel = shutdown.clone();
            tokio::spawn(upgrade::watchdog::run_watchdog_periodic(
                install_root, cfg_clone, tx, cancel,
            ));

            // Healthy-beat recorder: stamps `state.json.last_health_at`
            // every 15 s so the periodic watchdog can promote
            // `PendingHealth` → `Stable` once the boot-health window
            // expires with continuous beats.
            spawn_health_beat_recorder(up_cfg.install_root.clone(), shutdown.clone());
        }
    }

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

    // Capability strings advertised on every health heartbeat. The
    // manager UI gates per-feature surfaces (e.g. the Upgrade button)
    // on these. `"upgrade"` is unconditional — the SDK upgrade module
    // is always compiled in, mirroring the edge's baseline. When the
    // operator hasn't wired `[upgrade]` in the TOML, the runtime
    // dispatch (`dispatch_upgrade_binary`) returns `upgrade_disabled`
    // with a pointer at the missing config; the button stays visible
    // either way so operators can discover the feature.
    let capabilities: Vec<&'static str> = vec!["upgrade"];

    // Spawn discovery + polling startup in the background. While discovery
    // retries, this task emits a health heartbeat with
    // `gateway_target.reachable = false` on every failed attempt so the
    // manager's two-dot card renders Sidecar Online / Target Unreachable.
    // Once discovery succeeds we install the real command handler and hand
    // over to the steady-state polling engine, which then owns reachability
    // tracking via its own alarm-poll heartbeat.
    let discovery_client = appear_client.clone();
    let discovery_cfg_polling = cfg.polling.clone();
    let discovery_appear_x_cfg = cfg.appear_x.clone();
    let discovery_target_address = cfg.appear_x.address.clone();
    let discovery_emitter = emitter.clone();
    let discovery_handler_slot = deferred_handler.clone();
    let discovery_cancel = shutdown.clone();
    let discovery_upgrade_coord = upgrade_coord.clone();
    let discovery_capabilities = capabilities.clone();
    // Shared "last classified error" published by the discovery loop and
    // read by the heartbeat task — keeps the two tasks decoupled so the
    // heartbeat runs on a steady 10 s cadence regardless of how long each
    // discovery attempt takes (a 10 s JSON-RPC timeout would otherwise
    // collapse the heartbeat cadence down to ~15 s and lose the race
    // against the SDK's 15 s default heartbeat).
    let discovery_phase = Arc::new(tokio::sync::RwLock::new(Some((
        0u32,
        "tcp_refused".to_string(),
    ))));

    // Heartbeat task: emits `gateway_target.reachable = false` every 10 s
    // for as long as `discovery_phase` is `Some(...)`. Tighter than the
    // SDK's 15 s default heartbeat so the rich envelope reliably wins the
    // overwrite race in the manager's `cached_health`.
    let hb_emitter = emitter.clone();
    let hb_phase = discovery_phase.clone();
    let hb_target_address = cfg.appear_x.address.clone();
    let hb_cancel = shutdown.clone();
    let hb_capabilities = capabilities.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = hb_cancel.cancelled() => return,
            }
            let phase = hb_phase.read().await.clone();
            let Some((failures, error_code)) = phase else {
                // Discovery succeeded — polling owns the heartbeat now.
                return;
            };
            let target = bilbycast_gateway_sdk::GatewayTargetHealth {
                reachable: false,
                target_address: hb_target_address.clone(),
                gateway_host: appear_x::reachability::detect_hostname(),
                gateway_egress_ip: appear_x::reachability::detect_egress_ip(),
                last_successful_poll_unix: None,
                last_error_code: Some(error_code),
                consecutive_failures: Some(failures),
            };
            let health = serde_json::json!({
                "status": "critical",
                "alarms": [],
                "version": env!("CARGO_PKG_VERSION"),
                "capabilities": hb_capabilities,
            });
            if let Err(e) = hb_emitter.emit_health_with_target(health, target).await {
                debug!("Failed to emit discovery-phase health: {e}");
            }
        }
    });

    let discovery_phase_for_loop = discovery_phase.clone();
    tokio::spawn(async move {
        info!("Running Appear X capability discovery in background…");
        let cards_mmi_versions = ["2.8", "2.16", "4.1", "1.0"];
        let retry_delay = std::time::Duration::from_secs(5);
        let mut attempt: u32 = 0;
        let caps = loop {
            match appear_x::capabilities::discover(&discovery_client, &cards_mmi_versions).await {
                Ok(c) => {
                    info!("Capability discovery: {}", c.summary());
                    break c;
                }
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    let error_code = appear_x::reachability::classify_jsonrpc_error(&e).to_string();
                    warn!(
                        "Capability discovery failed (attempt {attempt}): {e:#}. \
                         Retrying in {} s",
                        retry_delay.as_secs()
                    );
                    {
                        let mut g = discovery_phase_for_loop.write().await;
                        *g = Some((attempt, error_code));
                    }

                    tokio::select! {
                        _ = tokio::time::sleep(retry_delay) => {}
                        _ = discovery_cancel.cancelled() => {
                            info!("Shutdown requested during capability discovery");
                            return;
                        }
                    }
                }
            }
        };

        // Discovery succeeded — flip the phase to None so the heartbeat
        // task exits and the polling engine below takes over.
        {
            let mut g = discovery_phase_for_loop.write().await;
            *g = None;
        }

        // Build steady-state plumbing now that we know the chassis layout.
        let shared_state = appear_x::state::SharedAppearXState::new(
            caps.clone(),
            env!("CARGO_PKG_VERSION"),
            discovery_target_address.clone(),
        );
        let mmi_versions = appear_x::commands::MmiVersions::from(&discovery_cfg_polling);
        let real_handler = Arc::new(appear_x::commands::AppearXCommandHandler::new(
            discovery_client.clone(),
            shared_state.clone(),
            mmi_versions,
            discovery_upgrade_coord,
        ));
        discovery_handler_slot.install(real_handler).await;

        let polling_identity = appear_x::polling::GatewayIdentity {
            target_address: discovery_target_address,
            gateway_host: appear_x::reachability::detect_hostname(),
            failure_threshold: discovery_appear_x_cfg.reachability_failure_threshold,
            event_dwell_secs: discovery_appear_x_cfg.reachability_event_dwell_secs,
            capabilities: discovery_capabilities,
        };
        if let Err(e) = appear_x::polling::run_polling(
            discovery_client,
            discovery_cfg_polling,
            caps,
            shared_state,
            discovery_emitter,
            polling_identity,
            discovery_cancel,
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

/// Drain `upgrade_event_rx` and forward each [`UpgradeEvent`] to the manager
/// over the SDK's [`Emitter`] as a [`GatewayEvent`]. Mirrors the edge's
/// `manager::events::EventSender` upgrade-event categorisation but lives
/// here instead of inside the SDK so the SDK stays Emitter-agnostic.
///
/// Lives for the process lifetime; cancelled via the shared shutdown token.
fn spawn_upgrade_event_forwarder(
    mut upgrade_event_rx: tokio::sync::mpsc::Receiver<UpgradeEvent>,
    emitter: Emitter,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                maybe = upgrade_event_rx.recv() => {
                    let Some(ev) = maybe else { return; };
                    let severity = match ev.severity {
                        "critical" => EventSeverity::Critical,
                        "major"    => EventSeverity::Major,
                        "minor"    => EventSeverity::Minor,
                        _          => EventSeverity::Info,
                    };
                    // Build details with the structured fields the manager
                    // UI looks for (`error_code`, version pair, channel,
                    // optional size).
                    let mut details = serde_json::Map::new();
                    details.insert("error_code".to_string(), json!(ev.error_code));
                    if let Some(v) = ev.from_version.as_ref() { details.insert("from_version".into(), json!(v)); }
                    if let Some(v) = ev.to_version.as_ref()   { details.insert("to_version".into(),   json!(v)); }
                    if let Some(c) = ev.channel.as_ref()      { details.insert("channel".into(),      json!(c)); }
                    if let Some(b) = ev.size_bytes            { details.insert("size_bytes".into(),   json!(b)); }
                    let event = GatewayEvent::new(severity, "upgrade", ev.message)
                        .with_error_code(ev.error_code)
                        .with_details(serde_json::Value::Object(details));
                    if let Err(e) = emitter.emit_event(event).await {
                        debug!("upgrade event forwarder: emit failed: {e}");
                    }
                }
            }
        }
    });
}

/// Periodically tap the upgrade state file with a healthy-beat marker so
/// the periodic watchdog can promote `PendingHealth` → `Stable`. Wired
/// to the alarms-poll heartbeat cadence — the same heartbeat the
/// reachability tracker uses, so a "manager+chassis both happy" beat is
/// what drives finalisation. Implemented by `record_healthy_beat`
/// in the SDK; threaded in here as a separate light tokio task so the
/// polling engine doesn't need to know about upgrades.
fn spawn_health_beat_recorder(
    install_root: std::path::PathBuf,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tick.tick() => {
                    upgrade::watchdog::record_healthy_beat(&install_root);
                }
            }
        }
    });
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
