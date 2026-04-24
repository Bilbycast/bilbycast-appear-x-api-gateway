// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub manager: ManagerConfig,
    pub appear_x: AppearXConfig,
    pub polling: PollingConfig,
}

/// Operator-facing `[manager]` section of the gateway's TOML config.
///
/// This shape is intentionally a superset of the SDK's `GatewayConfig`: the
/// SDK owns validation of the WS-facing fields (URLs, certs, credentials),
/// and `credentials_file` is the gateway's own disk-path for the
/// `CredentialStore`.
#[derive(Debug, Clone, Deserialize)]
pub struct ManagerConfig {
    /// Ordered list of manager WebSocket URLs (each `wss://`, 1–16
    /// entries). The SDK rotates through them on WS close with the
    /// standard reconnect backoff. Single-instance deployments still
    /// use a one-element array.
    pub urls: Vec<String>,
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

    /// Polling interval (seconds) for the fast `Xger:*/cardStatus/GetCardStatus`
    /// poll per populated slot. Drives the broadcast-engineer Card Health
    /// panel on the manager — keep short (≤ 5 s) so PTP drop / SFP RX power
    /// loss surfaces quickly.
    #[serde(default = "default_5")]
    pub card_status_interval_secs: u64,

    /// Polling interval (seconds) for the slower Xger Get* calls (coder
    /// services, multi services, audio profiles, IP interfaces, card
    /// allocations, pool config). These are config surfaces — they only
    /// change when the operator changes them.
    #[serde(default = "default_30")]
    pub xger_config_interval_secs: u64,

    /// Rx optical-power threshold in dBm. When *any* populated optical port
    /// drops below this, the gateway emits a Minor `sfp_low_rx_power` event.
    /// The industry-standard SFP+ receiver sensitivity floor is around
    /// -14 dBm for 10 G-SR; -18 dBm is the default early-warning trigger.
    #[serde(default = "default_rx_threshold")]
    pub sfp_low_rx_dbm_threshold: f64,

    /// Maximum SFP cage temperature (°C) before the gateway emits a Minor
    /// `sfp_high_temperature` event. QSFP+ modules typically list 70–75 °C
    /// as the commercial limit; 70 °C is a conservative early warning.
    #[serde(default = "default_temp_threshold")]
    pub sfp_high_temp_c_threshold: f64,
}

fn default_10() -> u64 { 10 }
fn default_15() -> u64 { 15 }
fn default_30() -> u64 { 30 }
fn default_5() -> u64 { 5 }
fn default_alarms_mmi_version() -> String { "2.8".into() }
fn default_chassis_mmi_version() -> String { "4.1".into() }
fn default_cards_mmi_version() -> String { "2.8".into() }
fn default_rx_threshold() -> f64 { -18.0 }
fn default_temp_threshold() -> f64 { 70.0 }

impl AppConfig {
    /// Load the config, optionally skipping the manager URL validation.
    /// `skip_manager_validation = true` is used by the `probe` subcommand,
    /// which talks only to the Appear X unit and never connects to the manager.
    ///
    /// Note: the SDK's `GatewayConfig::validate()` re-runs the WS-facing
    /// checks at connect time; the validation here is a friendlier
    /// early failure for misconfigured deployments.
    pub fn load_for_command(path: &Path, skip_manager_validation: bool) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: AppConfig = toml::from_str(&contents)
            .with_context(|| "Failed to parse TOML configuration")?;

        if !skip_manager_validation {
            if config.manager.urls.is_empty() {
                anyhow::bail!("Manager urls[] cannot be empty");
            }
            if config.manager.urls.len() > 16 {
                anyhow::bail!(
                    "Manager urls[] may contain at most 16 entries (got {})",
                    config.manager.urls.len()
                );
            }
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for (i, url) in config.manager.urls.iter().enumerate() {
                if !url.starts_with("wss://") {
                    anyhow::bail!(
                        "Manager urls[{i}] = {url:?} must use wss:// (TLS). \
                         Plaintext ws:// connections are not allowed."
                    );
                }
                if url.len() > 2048 {
                    anyhow::bail!(
                        "Manager urls[{i}] must be at most 2048 characters"
                    );
                }
                if !seen.insert(url.as_str()) {
                    anyhow::bail!("Manager urls[{i}] = {url:?} is a duplicate");
                }
            }
        }
        if config.appear_x.address.is_empty() {
            anyhow::bail!("Appear X address must not be empty");
        }

        Ok(config)
    }
}
