// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! Runtime device discovery / capability detection for the Appear X platform.
//!
//! Different Appear chassis (X5, X10, X20) hold different combinations of
//! card software (`net.appear.x5.hevc-sdi`, `net.appear.x5.jpegxs-sdi`, IP
//! Gateway boards, IP 2110 encoders, …), and each card software exposes its
//! own versioned JSON-RPC interfaces under the board endpoint. There is no
//! introspection method that lists installed interfaces, so on startup we:
//!
//! 1. Query the chassis-level `cards/GetChassisInfo` and `cards/GetCardStates`
//!    to learn the chassis type, the per-slot card name / serial / software
//!    id / feature list, and the running software version.
//! 2. For each populated slot, walk the entries in [`crate::appear_x::probe_registry`]
//!    and try each `(interface, version, module, command)` quadruple. The
//!    first version that returns a JSON-RPC `result` is recorded.
//!
//! The resulting [`DeviceCapabilities`] is then handed to the polling engine
//! and the WebSocket client so the gateway only ever issues calls that this
//! particular firmware actually understands.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use tracing::{debug, info, warn};

use super::jsonrpc::JsonRpcClient;
use super::probe_registry::{ProbeParams, CARD_PROBES};

/// Top-level capability map for the Appear unit.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCapabilities {
    /// Chassis identifier reported by `cards/GetChassisInfo`, e.g. `X20_2RU`,
    /// `X10_1RU`, `X5_1RU`. Used to set the device sub-type reported to the
    /// manager.
    pub chassis_type: String,
    /// MMI interface version that successfully answered `cards/GetChassisInfo`.
    pub cards_mmi_version: String,
    /// Per-slot capability records, keyed by slot number for stable ordering.
    pub slots: BTreeMap<u32, SlotCapabilities>,
}

/// Per-slot capability record.
#[derive(Debug, Clone, Serialize)]
pub struct SlotCapabilities {
    pub slot: u32,
    /// Card model name from `cards/GetChassisInfo` (e.g. `X5`).
    pub name: String,
    /// Hardware serial number.
    pub serial: String,
    /// Card software identifier (e.g. `net.appear.x5.hevc-sdi`).
    pub software_id: Option<String>,
    /// Human-readable software display name (e.g. `X5 HEVC SDI`).
    pub software_display_name: Option<String>,
    /// Running software version string.
    pub software_version: Option<String>,
    /// Feature flags reported for this card (e.g. `["sdi-hybrid", "ted",
    /// "encoder", "decoder", "ipinput", "ipoutput", "mmi", "srt"]`).
    pub features: Vec<String>,
    /// Interfaces successfully discovered on this slot. Keyed by interface
    /// name; the value is the version that responded plus the canonical
    /// `<iface>:<ver>/<module>/<command>` string used during the probe.
    pub discovered_interfaces: BTreeMap<String, DiscoveredInterfaceRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredInterfaceRecord {
    pub family: String,
    pub version: String,
    pub probe_method: String,
}

/// Run the full discovery sequence against an Appear unit.
///
/// `cards_mmi_versions` is the list of MMI interface versions to try for the
/// chassis-level `cards/*` calls. The first one that responds wins. The default
/// list `["2.8", "2.16", "4.1", "1.0"]` covers everything we've seen on
/// firmware in the wild.
pub async fn discover(
    client: &JsonRpcClient,
    cards_mmi_versions: &[&str],
) -> Result<DeviceCapabilities> {
    // Authenticate first; subsequent calls reuse the cached session token.
    client
        .authenticate()
        .await
        .context("Failed to authenticate with Appear unit during discovery")?;

    // Step 1: chassis info — try each MMI version until one answers.
    let (chassis_info, mmi_version) = {
        let mut found = None;
        for v in cards_mmi_versions {
            let method = format!("mmi:{v}/cards/GetChassisInfo");
            match client.call_mmi(&method, json!({})).await {
                Ok(result) => {
                    debug!("Chassis info responded at MMI version {v}");
                    found = Some((result, (*v).to_string()));
                    break;
                }
                Err(e) => {
                    debug!("MMI version {v} did not respond to GetChassisInfo: {e}");
                }
            }
        }
        found.context(
            "None of the configured MMI versions responded to cards/GetChassisInfo. \
             Set polling.cards_mmi_version explicitly in config.toml.",
        )?
    };

    let data = chassis_info
        .get("data")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let chassis_type = data
        .get("chassisType")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    info!(
        "Detected Appear chassis: type={chassis_type} (mmi:{mmi_version}/cards/GetChassisInfo)"
    );

    // Step 2: card states (per-slot software_id, version, login state).
    let card_states_method = format!("mmi:{mmi_version}/cards/GetCardStates");
    let card_states_value = client
        .call_mmi(&card_states_method, json!({}))
        .await
        .unwrap_or_else(|e| {
            warn!("cards/GetCardStates failed: {e} — software ids will be missing");
            json!({})
        });

    // Index card_states by slot for the merge below.
    let card_states_by_slot: BTreeMap<u32, serde_json::Value> = card_states_value
        .get("cards")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let slot = c.get("slot").and_then(|s| s.as_u64()).map(|s| s as u32)?;
                    Some((slot, c.clone()))
                })
                .collect()
        })
        .unwrap_or_default();

    // Step 3: walk cards from chassisInfo and build per-slot records.
    let mut slots = BTreeMap::new();
    if let Some(cards) = data.get("cards").and_then(|c| c.as_array()) {
        for entry in cards {
            let value = match entry.get("value") {
                Some(v) => v,
                None => continue,
            };
            let slot = match value.get("slot").and_then(|s| s.as_u64()) {
                Some(s) => s as u32,
                None => continue,
            };
            let name = value
                .get("name")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let serial = value
                .get("serial")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let features = value
                .get("features")
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|f| f.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            // Software info comes from card_states. Look it up by slot.
            let (software_id, software_display_name, software_version) =
                extract_software_info(card_states_by_slot.get(&slot));

            slots.insert(
                slot,
                SlotCapabilities {
                    slot,
                    name,
                    serial,
                    software_id,
                    software_display_name,
                    software_version,
                    features,
                    discovered_interfaces: BTreeMap::new(),
                },
            );
        }
    }

    if slots.is_empty() {
        warn!(
            "cards/GetChassisInfo returned no card entries — the unit may be uninitialised"
        );
    }

    // Step 4: probe known card interfaces per slot.
    for (slot, caps) in slots.iter_mut() {
        info!(
            "Probing card interfaces on slot {} ({}, software_id={:?})",
            slot, caps.name, caps.software_id
        );
        for entry in CARD_PROBES {
            for version in entry.versions {
                let method = format!(
                    "{}:{}/{}/{}",
                    entry.interface, version, entry.module, entry.command
                );
                let params = match entry.params {
                    ProbeParams::Empty => json!({}),
                    ProbeParams::EmptyQuery => json!({"query": {}}),
                };
                match client.call_board(*slot, &method, params).await {
                    Ok(_) => {
                        debug!("  ✓ slot {} {}", slot, method);
                        caps.discovered_interfaces.insert(
                            entry.interface.to_string(),
                            DiscoveredInterfaceRecord {
                                family: entry.family.to_string(),
                                version: (*version).to_string(),
                                probe_method: method,
                            },
                        );
                        break; // stop trying older versions for this entry
                    }
                    Err(e) => {
                        // Most failures here are "Method not found" which is
                        // expected — that's how we discover what's missing.
                        // Log at debug only.
                        debug!("  ✗ slot {} {}: {}", slot, method, e);
                    }
                }
            }
        }
        info!(
            "  Slot {} discovered {} card interface(s)",
            slot,
            caps.discovered_interfaces.len()
        );
    }

    Ok(DeviceCapabilities {
        chassis_type,
        cards_mmi_version: mmi_version,
        slots,
    })
}

fn extract_software_info(
    state: Option<&serde_json::Value>,
) -> (Option<String>, Option<String>, Option<String>) {
    let state = match state {
        Some(s) => s,
        None => return (None, None, None),
    };
    // The structure is cards[].logical.value.login.value.swInfo.value
    // (running software) or cards[].logical.value.configSwInfo.value
    // (configured software). Prefer the running one.
    let sw_info = state
        .pointer("/logical/value/login/value/swInfo/value")
        .or_else(|| state.pointer("/logical/value/configSwInfo/value"));
    let sw_info = match sw_info {
        Some(v) => v,
        None => return (None, None, None),
    };
    let id = sw_info
        .get("softwareId")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let display = sw_info
        .get("displayName")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let ver = sw_info
        .get("ver")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    (id, display, ver)
}

impl DeviceCapabilities {
    /// One-line summary suitable for human-facing logs and the probe report.
    pub fn summary(&self) -> String {
        let total_ifaces: usize = self
            .slots
            .values()
            .map(|s| s.discovered_interfaces.len())
            .sum();
        format!(
            "chassis={} mmi:{} slots={} discovered_interfaces={}",
            self.chassis_type,
            self.cards_mmi_version,
            self.slots.len(),
            total_ifaces
        )
    }
}
