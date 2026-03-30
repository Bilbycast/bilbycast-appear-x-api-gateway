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
    #[serde(default = "default_30")]
    pub services_interval_secs: u64,
    /// Board slots to monitor
    #[serde(default)]
    pub boards: Vec<BoardConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BoardConfig {
    pub slot: u32,
    #[serde(default = "default_ip_gateway")]
    pub interface: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
}

fn default_10() -> u64 { 10 }
fn default_15() -> u64 { 15 }
fn default_30() -> u64 { 30 }
fn default_ip_gateway() -> String { "ipGateway".into() }
fn default_api_version() -> String { "1.15".into() }

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: AppConfig = toml::from_str(&contents)
            .with_context(|| "Failed to parse TOML configuration")?;

        // Validate
        if !config.manager.url.starts_with("wss://") {
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
