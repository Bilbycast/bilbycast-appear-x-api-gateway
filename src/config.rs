// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub manager: ManagerConfig,
    pub appear_x: AppearXConfig,
    pub polling: PollingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManagerConfig {
    /// WebSocket URL for the manager (must be wss://)
    pub url: String,
    /// One-time registration token (cleared after first registration)
    pub registration_token: Option<String>,
    /// Path to file where node_id + node_secret are persisted after registration
    #[serde(default = "default_credentials_file")]
    pub credentials_file: String,
    /// Accept self-signed TLS certs for manager connection (requires BILBYCAST_ALLOW_INSECURE=1)
    #[serde(default)]
    pub accept_self_signed_cert: bool,
    /// SHA-256 certificate fingerprint for cert pinning (colon-separated hex)
    #[serde(default)]
    pub cert_fingerprint: Option<String>,
}

fn default_credentials_file() -> String {
    "credentials.json".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppearXConfig {
    /// IP address or hostname of the Appear X unit
    pub address: String,
    /// Login username (typically "admin")
    pub username: String,
    /// Login password
    pub password: String,
    /// Accept self-signed HTTPS certs on the Appear X unit
    #[serde(default = "default_true")]
    pub accept_self_signed_cert: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollingConfig {
    #[serde(default = "default_10")]
    pub alarms_interval_secs: u64,
    #[serde(default = "default_30")]
    pub chassis_interval_secs: u64,
    #[serde(default = "default_15")]
    pub inputs_interval_secs: u64,
    #[serde(default = "default_15")]
    pub outputs_interval_secs: u64,
    /// MMI interface version used for alarms calls (e.g. "2.8", "2.16").
    /// Different Appear firmware versions expose different MMI interface versions.
    #[serde(default = "default_alarms_mmi_version")]
    pub alarms_mmi_version: String,
    /// MMI interface version used for chassisModel calls (e.g. "4.1", "2.16").
    #[serde(default = "default_chassis_mmi_version")]
    pub chassis_mmi_version: String,
    /// MMI interface version used for `cards/*` calls (GetChassisInfo, GetCardStates).
    #[serde(default = "default_cards_mmi_version")]
    pub cards_mmi_version: String,
    /// Polling interval (seconds) for `cards/GetChassisInfo` + `cards/GetCardStates`.
    #[serde(default = "default_30")]
    pub cards_interval_secs: u64,
}

fn default_10() -> u64 { 10 }
fn default_15() -> u64 { 15 }
fn default_30() -> u64 { 30 }
fn default_alarms_mmi_version() -> String { "2.8".into() }
fn default_chassis_mmi_version() -> String { "4.1".into() }
fn default_cards_mmi_version() -> String { "2.8".into() }

impl AppConfig {
    /// Load the config, optionally skipping the manager URL validation.
    /// `skip_manager_validation = true` is used by the `probe` subcommand,
    /// which talks only to the Appear X unit and never connects to the manager.
    pub fn load_for_command(path: &Path, skip_manager_validation: bool) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: AppConfig = toml::from_str(&contents)
            .with_context(|| "Failed to parse TOML configuration")?;

        if !skip_manager_validation && !config.manager.url.starts_with("wss://") {
            anyhow::bail!(
                "Manager URL must use wss:// (TLS). Plaintext ws:// connections are not allowed."
            );
        }
        if config.appear_x.address.is_empty() {
            anyhow::bail!("Appear X address must not be empty");
        }

        Ok(config)
    }
}
