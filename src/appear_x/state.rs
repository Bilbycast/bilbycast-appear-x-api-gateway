// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Shared in-memory snapshot of the latest Appear X state.
//!
//! Polling tasks write their slice into this struct as fresh data arrives;
//! a single emitter task ticks periodically, takes a flattened snapshot, and
//! sends one consolidated `stats` message to the manager. This avoids the
//! "last payload wins" problem on the manager side, where every separate
//! `stats` message would overwrite the previous one in `cached_stats`.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use super::capabilities::{DeviceCapabilities, SlotCapabilities};

/// Latest known state of the Appear X unit, populated by the polling tasks.
#[derive(Debug, Default)]
pub struct AppearXState {
    pub alarms: Vec<Value>,
    /// Alarm IDs from the previous poll, used for change detection.
    pub prev_alarm_ids: HashSet<String>,
    pub status: String, // "ok" | "degraded" | "critical"
    pub chassis: Option<Value>,
    pub chassis_info: Option<Value>,
    pub card_states: Vec<Value>,
    // ─ ipGateway (legacy IP Gateway boards) ─
    pub inputs: BTreeMap<u32, Vec<Value>>,
    pub outputs: BTreeMap<u32, Vec<Value>>,
    pub ip_interfaces: BTreeMap<u32, Vec<Value>>,
    // ─ Xger (X5/X10/X20 card-manager surface) ─
    /// Raw `Xger:*/cardStatus/GetCardStatus` result per slot — carries PTP
    /// lock, NMOS registry status, QSFP/SFP diagnostics, and physicalPort
    /// runtime status. Fast-polled (every ~5 s) so broadcast engineers see
    /// live signal state without a page refresh.
    pub card_status: BTreeMap<u32, Value>,
    /// `Xger:*/coderService/GetCoderServices` — configured encoder/decoder
    /// services on the card. Present once the card is commissioned; empty
    /// on a bare X5 HEVC SDI.
    pub coder_services: BTreeMap<u32, Vec<Value>>,
    /// `Xger:*/multiService/GetMultiServices` — multi-service (MPTS-style)
    /// definitions used as encoder/decoder sources.
    pub multi_services: BTreeMap<u32, Vec<Value>>,
    /// `Xger:*/audioProfile/GetAudioProfiles` — audio encode/decode profiles
    /// referenced by coder services.
    pub audio_profiles: BTreeMap<u32, Vec<Value>>,
    /// `Xger:*/ipInterface/GetIpInterfaces` — IP interfaces defined on the
    /// card manager (distinct from ipGateway board `ip_interfaces`).
    pub xger_ip_interfaces: BTreeMap<u32, Vec<Value>>,
    /// `Xger:*/cardAllocation/GetCardAllocations` — card-pool allocations.
    pub card_allocations: BTreeMap<u32, Vec<Value>>,
    /// `Xger:*/poolConfig/GetPoolConfig` — pool configuration.
    pub pool_config: BTreeMap<u32, Value>,
    /// `Xger:*/ipConnection/GetIpConnections` — Phase 2: actual IP transport
    /// bindings (UDP / RTP / SRT / RIST). Where SRT-mode (caller / listener /
    /// rendezvous), latency, encryption passphrase, FEC mode all live.
    pub ip_connections: BTreeMap<u32, Vec<Value>>,
    /// `Xger:*/redundancyGroup/GetRedundancyGroups` — Phase 2: ST 2022-7 /
    /// hot-standby redundancy group config. Pairs two ipInterfaces into one
    /// logical leg.
    pub redundancy_groups: BTreeMap<u32, Vec<Value>>,
    /// `Xger:*/redundancyGroupStatus/GetRedundancyGroupStatus` — Phase 2: live
    /// status of every configured redundancy group (active leg, switch count).
    pub redundancy_group_status: BTreeMap<u32, Value>,
    /// Phase 3a: per-encoder runtime config blobs from
    /// `hipEnc:*/hipEncoder/GetEncoders` /
    /// `hipTsEnc:*/hipTsEncoder/GetEncoders` /
    /// `hipDec:*/hipDecoder/GetDecoders` /
    /// `hipTsDec:*/hipTsDecoder/GetDecoders`. Per-slot raw blob; the manager UI
    /// surfaces an Edit-JSON modal because the schema varies by family and
    /// firmware. Operator runs Get → mutate → Set for live bitrate hot-changes,
    /// intra-period overrides, dynamic-range tweaks.
    pub hip_encoders: BTreeMap<u32, Value>,
    pub hip_decoders: BTreeMap<u32, Value>,
    /// Phase 3b: SCTE-35 / DPI / ESAM splicing surface. Each module is a
    /// raw per-slot blob — schemas are stable enough that a JSON-textarea
    /// editor is reasonable, but the manager UI also exposes a structured
    /// splice-history viewer fed by the on-demand `get_scte35_history`
    /// command (NOT polled — log can be large).
    pub dpi_status: BTreeMap<u32, Value>,
    pub esam_status: BTreeMap<u32, Value>,
    pub pois_server_status: BTreeMap<u32, Value>,
    /// `Xger:*/lockStatus/GetLockStatus` — input lock status for each
    /// encoder. Present only on commissioned units.
    pub lock_status: BTreeMap<u32, Value>,
    /// `Xger:*/psiStatus/GetPsiStatus` — decoded PSI/SI tables from active
    /// TS inputs. Present only on commissioned units.
    pub psi_status: BTreeMap<u32, Value>,
    // ─ board (cross-board services) ─
    /// `board:{ver}/services/GetOutputServices` per slot — the native way
    /// the Appear X card-manager exposes configured IP outputs on X5 /
    /// X10 / X20 cards where `Xger:*/ipConnection` isn't loaded. Each
    /// entry has `nodeType: Flow::FlowSink::IpOutput::`, a `body` JSON
    /// string with `{slot, address, port, vlan}`, a `label`, and a
    /// `sources` list pointing back at the input service feeding it.
    /// SMPTE 2022-7 redundant pairs surface as two entries with the same
    /// `label` / `name` but different `body.address` — the manager UI
    /// groups them visually as one logical output with A/B legs.
    pub output_services: BTreeMap<u32, Vec<Value>>,
}

#[derive(Clone)]
pub struct SharedAppearXState {
    inner: Arc<RwLock<AppearXState>>,
    /// Static capability snapshot (chassis type, per-slot card metadata)
    /// captured at startup. Used to populate the `slots` and `chassis_model`
    /// fields of every consolidated stats payload, so the manager has board
    /// names and software versions to render even before the first
    /// `cards/GetChassisInfo` poll has fired.
    caps: Arc<DeviceCapabilities>,
    /// Process start time, used to derive `uptime_secs` for the snapshot.
    started_at: Instant,
    /// Software version reported in every snapshot.
    version: &'static str,
    /// Appear X unit address (e.g. "192.168.50.8") for display in the manager UI.
    appear_x_address: String,
}

impl SharedAppearXState {
    pub fn new(caps: DeviceCapabilities, version: &'static str, appear_x_address: String) -> Self {
        let initial = AppearXState {
            status: "ok".to_string(),
            ..AppearXState::default()
        };
        Self {
            inner: Arc::new(RwLock::new(initial)),
            caps: Arc::new(caps),
            started_at: Instant::now(),
            version,
            appear_x_address,
        }
    }

    /// Update alarms and return (new_alarms, cleared_alarm_ids) for event
    /// forwarding. An alarm is "new" if its `alarmId` was not in the previous
    /// poll; "cleared" if its previous `alarmId` is no longer present.
    pub async fn set_alarms(
        &self,
        alarms: Vec<Value>,
        status: &str,
    ) -> (Vec<Value>, Vec<String>) {
        let mut g = self.inner.write().await;

        let current_ids: HashSet<String> = alarms
            .iter()
            .filter_map(|a| a.get("alarmId").and_then(|v| v.as_str()).map(String::from))
            .collect();

        let new_alarms: Vec<Value> = alarms
            .iter()
            .filter(|a| {
                a.get("alarmId")
                    .and_then(|v| v.as_str())
                    .map(|id| !g.prev_alarm_ids.contains(id))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        let cleared_ids: Vec<String> = g
            .prev_alarm_ids
            .iter()
            .filter(|id| !current_ids.contains(id.as_str()))
            .cloned()
            .collect();

        g.prev_alarm_ids = current_ids;
        g.alarms = alarms;
        g.status = status.to_string();

        (new_alarms, cleared_ids)
    }

    pub async fn set_chassis(&self, chassis: Value) {
        let mut g = self.inner.write().await;
        g.chassis = Some(chassis);
    }

    pub async fn set_cards(&self, info: Value, states: Value) {
        let mut g = self.inner.write().await;
        g.chassis_info = Some(info);
        g.card_states = states.as_array().cloned().unwrap_or_default();
    }

    pub async fn set_slot_inputs(&self, slot: u32, inputs: Value) {
        let mut g = self.inner.write().await;
        g.inputs
            .insert(slot, inputs.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_slot_outputs(&self, slot: u32, outputs: Value) {
        let mut g = self.inner.write().await;
        g.outputs
            .insert(slot, outputs.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_slot_ip_interfaces(&self, slot: u32, ifaces: Value) {
        let mut g = self.inner.write().await;
        g.ip_interfaces
            .insert(slot, ifaces.as_array().cloned().unwrap_or_default());
    }

    /// Set the raw `cardStatus` blob for a slot and return the prior value so
    /// the caller can derive edge-triggered events (PTP unlocked, SFP RX
    /// power drop, SFP overtemp).
    pub async fn set_card_status(&self, slot: u32, status: Value) -> Option<Value> {
        let mut g = self.inner.write().await;
        g.card_status.insert(slot, status)
    }

    pub async fn set_coder_services(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.coder_services
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_multi_services(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.multi_services
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_audio_profiles(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.audio_profiles
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_xger_ip_interfaces(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.xger_ip_interfaces
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_card_allocations(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.card_allocations
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_pool_config(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.pool_config.insert(slot, v);
    }

    pub async fn set_ip_connections(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.ip_connections
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_redundancy_groups(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.redundancy_groups
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    pub async fn set_redundancy_group_status(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.redundancy_group_status.insert(slot, v);
    }

    pub async fn set_hip_encoders(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.hip_encoders.insert(slot, v);
    }

    pub async fn set_hip_decoders(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.hip_decoders.insert(slot, v);
    }

    pub async fn set_dpi_status(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.dpi_status.insert(slot, v);
    }

    pub async fn set_esam_status(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.esam_status.insert(slot, v);
    }

    pub async fn set_pois_server_status(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.pois_server_status.insert(slot, v);
    }

    pub async fn set_lock_status(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.lock_status.insert(slot, v);
    }

    pub async fn set_psi_status(&self, slot: u32, v: Value) {
        let mut g = self.inner.write().await;
        g.psi_status.insert(slot, v);
    }

    /// Set the per-slot `GetOutputServices` reply (a flat array of IP-output
    /// service records). Extracts the outer `data.[]` envelope so the stored
    /// shape is homogeneous with [`set_slot_outputs`].
    pub async fn set_output_services(&self, slot: u32, items: Value) {
        let mut g = self.inner.write().await;
        g.output_services
            .insert(slot, items.as_array().cloned().unwrap_or_default());
    }

    /// Look up the discovered API version for a `<interface>/<module>` pair
    /// on a given slot. Returns `None` if the slot or module was not
    /// discovered.
    pub fn discovered_version(&self, slot: u32, interface: &str, module: &str) -> Option<String> {
        self.caps
            .slots
            .get(&slot)
            .and_then(|s| s.module_version(interface, module).map(|v| v.to_string()))
    }

    /// Build the consolidated stats payload to send to the manager.
    ///
    /// Per-slot maps are flattened to top-level arrays where each item carries
    /// a `"slot": N` field, so the manager driver's `extract_metrics` (which
    /// counts top-level `inputs.len()`, `outputs.len()`, etc.) sees totals
    /// across the whole chassis without needing to be slot-aware.
    pub async fn snapshot(&self) -> Value {
        let g = self.inner.read().await;

        // Flatten per-slot inputs/outputs/interfaces with a slot annotation.
        let inputs_flat = flatten_with_slot(&g.inputs);
        let outputs_flat = flatten_with_slot(&g.outputs);
        let ifaces_flat = flatten_with_slot(&g.ip_interfaces);

        // Xger family (card-manager) flattened same way.
        let coder_services_flat = flatten_with_slot(&g.coder_services);
        let multi_services_flat = flatten_with_slot(&g.multi_services);
        let audio_profiles_flat = flatten_with_slot(&g.audio_profiles);
        let xger_ip_interfaces_flat = flatten_with_slot(&g.xger_ip_interfaces);
        let card_allocations_flat = flatten_with_slot(&g.card_allocations);
        let output_services_flat = flatten_with_slot(&g.output_services);
        let ip_connections_flat = flatten_with_slot(&g.ip_connections);
        let redundancy_groups_flat = flatten_with_slot(&g.redundancy_groups);

        // card_status / pool_config / lock_status / psi_status are single
        // opaque objects per slot — deliver as slot-indexed maps so the
        // manager UI can render them without assuming a schema.
        let card_status_map = slot_map_to_json(&g.card_status);
        let pool_config_map = slot_map_to_json(&g.pool_config);
        let lock_status_map = slot_map_to_json(&g.lock_status);
        let psi_status_map = slot_map_to_json(&g.psi_status);
        let redundancy_group_status_map = slot_map_to_json(&g.redundancy_group_status);
        let hip_encoders_map = slot_map_to_json(&g.hip_encoders);
        let hip_decoders_map = slot_map_to_json(&g.hip_decoders);
        let dpi_status_map = slot_map_to_json(&g.dpi_status);
        let esam_status_map = slot_map_to_json(&g.esam_status);
        let pois_server_status_map = slot_map_to_json(&g.pois_server_status);

        // Health signals derived from card_status for easy metric extraction
        // on the manager side (and human-readable header badges).
        let health_signals = derive_health_signals(&g.card_status);

        // Slots from the static capability snapshot — board names, software
        // versions, and feature flags. Always present so the chassis card has
        // something to render even before the chassis_info poll lands.
        let slots: Vec<Value> = self
            .caps
            .slots
            .values()
            .map(slot_to_json)
            .collect();

        let uptime_secs = self.started_at.elapsed().as_secs();

        json!({
            "status": g.status,
            "version": self.version,
            "uptime_secs": uptime_secs,
            "chassis_model": self.caps.chassis_type,
            "appear_x_address": self.appear_x_address,
            "chassis": g.chassis.clone().unwrap_or(json!(null)),
            "chassis_info": g.chassis_info.clone().unwrap_or(json!(null)),
            "card_states": g.card_states,
            "slots": slots,
            "alarms": g.alarms,
            "inputs": inputs_flat,
            "outputs": outputs_flat,
            "ip_interfaces": ifaces_flat,
            "coder_services": coder_services_flat,
            "multi_services": multi_services_flat,
            "audio_profiles": audio_profiles_flat,
            "xger_ip_interfaces": xger_ip_interfaces_flat,
            "card_allocations": card_allocations_flat,
            "output_services": output_services_flat,
            "ip_connections": ip_connections_flat,
            "redundancy_groups": redundancy_groups_flat,
            "redundancy_group_status": redundancy_group_status_map,
            "card_status": card_status_map,
            "pool_config": pool_config_map,
            "lock_status": lock_status_map,
            "psi_status": psi_status_map,
            "hip_encoders": hip_encoders_map,
            "hip_decoders": hip_decoders_map,
            "dpi_status": dpi_status_map,
            "esam_status": esam_status_map,
            "pois_server_status": pois_server_status_map,
            "health_signals": health_signals,
        })
    }
}

fn flatten_with_slot(map: &BTreeMap<u32, Vec<Value>>) -> Vec<Value> {
    let mut out = Vec::new();
    for (slot, items) in map {
        for item in items {
            let mut o = item.clone();
            if let Some(obj) = o.as_object_mut() {
                obj.insert("slot".to_string(), json!(slot));
            }
            out.push(o);
        }
    }
    out
}

fn slot_map_to_json(map: &BTreeMap<u32, Value>) -> Value {
    let obj: serde_json::Map<String, Value> = map
        .iter()
        .map(|(slot, v)| (slot.to_string(), v.clone()))
        .collect();
    Value::Object(obj)
}

fn slot_to_json(s: &SlotCapabilities) -> Value {
    let mods: Vec<Value> = s
        .discovered_modules
        .values()
        .map(|r| {
            json!({
                "family": r.family,
                "interface": r.interface,
                "module": r.module,
                "version": r.version,
            })
        })
        .collect();
    json!({
        "slot": s.slot,
        "name": s.name,
        "serial": s.serial,
        "software_id": s.software_id,
        "software_display_name": s.software_display_name,
        "software_version": s.software_version,
        "features": s.features,
        "discovered_modules": mods,
    })
}

/// Distil the raw `cardStatus` blobs into a flat map of the broadcast-critical
/// signals: PTP lock state, NMOS registry health, worst SFP RX power, worst
/// SFP temperature. Drives the manager-side metric extractor and the Card
/// Health panel in the UI.
fn derive_health_signals(status_map: &BTreeMap<u32, Value>) -> Value {
    let mut by_slot = serde_json::Map::new();
    let mut global_ptp_locked = true;
    let mut global_ptp_seen = false;
    let mut global_rx_power_min: Option<f64> = None;
    let mut global_temp_max: Option<f64> = None;
    let mut global_nmos_ok = true;
    let mut global_nmos_seen = false;

    for (slot, status) in status_map {
        let mut slot_obj = serde_json::Map::new();

        // PTP lock — search a few likely locations. On X5 HEVC SDI, cardStatus
        // returns `"ptpLock": {}` when unlocked (empty) and a struct with
        // `state: LOCKED` when locked. Be defensive.
        let (ptp_locked, ptp_state) = extract_ptp_locked(status);
        if let Some(locked) = ptp_locked {
            global_ptp_seen = true;
            global_ptp_locked &= locked;
            slot_obj.insert("ptp_locked".into(), json!(locked));
            if let Some(st) = ptp_state {
                slot_obj.insert("ptp_state".into(), json!(st));
            }
        }

        // NMOS registry — `nmosStatus` is empty object when the registry is
        // not wired up; populated with `{connected: true, ...}` when it is.
        if let Some(nmos) = status.get("nmosStatus") {
            global_nmos_seen = true;
            let connected = nmos
                .get("connected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || nmos
                    .pointer("/value/connected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            global_nmos_ok &= connected;
            slot_obj.insert("nmos_registered".into(), json!(connected));
        }

        // QSFP / SFP diagnostics — iterate every port entry and find min RX
        // power + max temperature. `qsfpStatus.value` is an array of
        // `{key: portName, value: {diagnostics: {value: {temp, vcc, txPwr[], rxPwr[]}}}}`.
        let (rx_min, temp_max, ports) = extract_sfp_signals(status);
        if let Some(m) = rx_min {
            slot_obj.insert("qsfp_rx_power_dbm_min".into(), json!(m));
            global_rx_power_min = Some(match global_rx_power_min {
                Some(v) => v.min(m),
                None => m,
            });
        }
        if let Some(t) = temp_max {
            slot_obj.insert("qsfp_temp_c_max".into(), json!(t));
            global_temp_max = Some(match global_temp_max {
                Some(v) => v.max(t),
                None => t,
            });
        }
        if !ports.is_empty() {
            slot_obj.insert("qsfp_ports".into(), json!(ports));
        }

        by_slot.insert(slot.to_string(), Value::Object(slot_obj));
    }

    let mut global = serde_json::Map::new();
    if global_ptp_seen {
        global.insert("ptp_locked".into(), json!(global_ptp_locked));
    }
    if global_nmos_seen {
        global.insert("nmos_registered".into(), json!(global_nmos_ok));
    }
    if let Some(v) = global_rx_power_min {
        global.insert("qsfp_rx_power_dbm_min".into(), json!(v));
    }
    if let Some(v) = global_temp_max {
        global.insert("qsfp_temp_c_max".into(), json!(v));
    }

    json!({ "by_slot": Value::Object(by_slot), "global": Value::Object(global) })
}

fn extract_ptp_locked(status: &Value) -> (Option<bool>, Option<String>) {
    let pl = match status.get("ptpLock") {
        Some(v) => v,
        None => return (None, None),
    };
    // Empty object → no PTP signal known; surface as unknown (return None).
    if pl.is_object() && pl.as_object().map(|o| o.is_empty()).unwrap_or(false) {
        return (None, None);
    }
    // Look for a "state" field — values LOCKED / UNLOCKED / ACQUIRING.
    let state = pl
        .get("state")
        .or_else(|| pl.pointer("/value/state"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let locked = state.as_deref().map(|s| s.eq_ignore_ascii_case("LOCKED"));
    (locked, state)
}

/// Dive through every `qsfpStatus` / `sfpStatus` port, aggregate worst-case
/// RX-power and max temperature. Also produce a per-port summary vector.
///
/// On cables without optical diagnostics (e.g. the 40 G active-cable QSFP on
/// the X5 HEVC SDI testbed), `rxPwr` is `[0.0]` — a real dark optic reads
/// `≤ -40 dBm`. We treat `rxPwr == 0.0` as "no optic present" and skip it.
#[cfg(test)]
mod tests {
    use super::*;

    /// Sample `cardStatus` blob modelled on the live X20_2RU / X5 HEVC SDI
    /// response we recorded during probing. Port D3 is a 40 G active cable
    /// (`rxPwr=[0.0]`) so it registers no optic; that's the real-world shape.
    fn sample_card_status_no_optic() -> Value {
        json!({
            "physicalPortStatus": {},
            "ptpLock": {},
            "nmosStatus": {},
            "qsfpStatus": {"value": [{
                "key": "D3",
                "value": {
                    "vendorName": "APPEAR   ",
                    "vendorPN": "10005315 ",
                    "vendorSN": "125042800158 ",
                    "diagnostics": {
                        "value": { "temp": 42.3, "vcc": 3.36, "txPwr": [0.0], "rxPwr": [0.0] }
                    }
                }
            }]}
        })
    }

    fn sample_card_status_with_optic(rx_mw: f64, temp_c: f64, ptp_state: &str) -> Value {
        json!({
            "physicalPortStatus": {},
            "ptpLock": { "state": ptp_state },
            "nmosStatus": { "connected": true },
            "qsfpStatus": {"value": [{
                "key": "D3",
                "value": {
                    "vendorName": "APPEAR   ",
                    "vendorPN": "10005315 ",
                    "vendorSN": "125042800158 ",
                    "diagnostics": {
                        "value": { "temp": temp_c, "vcc": 3.36, "txPwr": [0.5], "rxPwr": [rx_mw] }
                    }
                }
            }]}
        })
    }

    #[test]
    fn health_signals_rollup_skips_dark_optic() {
        let mut map = BTreeMap::new();
        map.insert(1u32, sample_card_status_no_optic());
        let signals = derive_health_signals(&map);
        let global = signals.get("global").unwrap();
        // No optic present → no RX dBm rollup value.
        assert!(global.get("qsfp_rx_power_dbm_min").is_none());
        // Port D3 temp=42.3 bubbles up.
        assert!((global.get("qsfp_temp_c_max").unwrap().as_f64().unwrap() - 42.3).abs() < 0.1);
        // PTP empty object → unknown → not present.
        assert!(global.get("ptp_locked").is_none());
    }

    #[test]
    fn health_signals_rollup_with_optic() {
        // 0.1 mW → 10·log10(0.1) = -10 dBm. 60 °C is the hotter value.
        let mut map = BTreeMap::new();
        map.insert(1u32, sample_card_status_with_optic(0.1, 60.0, "LOCKED"));
        map.insert(2u32, sample_card_status_with_optic(0.01, 58.0, "LOCKED"));
        let signals = derive_health_signals(&map);
        let global = signals.get("global").unwrap();
        assert_eq!(global.get("ptp_locked").unwrap().as_bool(), Some(true));
        assert_eq!(global.get("nmos_registered").unwrap().as_bool(), Some(true));
        // Worst RX is slot 2 (0.01 mW → -20 dBm).
        let rx_min = global.get("qsfp_rx_power_dbm_min").unwrap().as_f64().unwrap();
        assert!((rx_min - (-20.0)).abs() < 0.5, "expected ~-20 dBm, got {rx_min}");
        // Hottest cage is slot 1 at 60 °C.
        let temp_max = global.get("qsfp_temp_c_max").unwrap().as_f64().unwrap();
        assert!((temp_max - 60.0).abs() < 0.5, "expected ~60 °C, got {temp_max}");
    }

    #[test]
    fn health_signals_ptp_partial_lock_rollup() {
        // One slot locked, one not → rollup.ptp_locked = false.
        let mut map = BTreeMap::new();
        map.insert(1u32, sample_card_status_with_optic(0.5, 45.0, "LOCKED"));
        map.insert(2u32, sample_card_status_with_optic(0.5, 45.0, "ACQUIRING"));
        let signals = derive_health_signals(&map);
        let global = signals.get("global").unwrap();
        assert_eq!(global.get("ptp_locked").unwrap().as_bool(), Some(false));
    }
}

fn extract_sfp_signals(status: &Value) -> (Option<f64>, Option<f64>, Vec<Value>) {
    let mut rx_min: Option<f64> = None;
    let mut temp_max: Option<f64> = None;
    let mut ports: Vec<Value> = Vec::new();

    let qsfp_arr = status
        .pointer("/qsfpStatus/value")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let sfp_arr = status
        .pointer("/sfpStatus/value")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    for entry in qsfp_arr.iter().chain(sfp_arr.iter()) {
        let port_name = entry
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let diag = entry.pointer("/value/diagnostics/value");
        let mut port_obj = serde_json::Map::new();
        port_obj.insert("port".into(), json!(port_name));
        if let Some(vendor) = entry.pointer("/value/vendorName").and_then(|v| v.as_str()) {
            port_obj.insert("vendor".into(), json!(vendor.trim().to_string()));
        }
        if let Some(pn) = entry.pointer("/value/vendorPN").and_then(|v| v.as_str()) {
            port_obj.insert("part_number".into(), json!(pn.trim().to_string()));
        }
        if let Some(sn) = entry.pointer("/value/vendorSN").and_then(|v| v.as_str()) {
            port_obj.insert("serial".into(), json!(sn.trim().to_string()));
        }
        if let Some(d) = diag {
            if let Some(temp) = d.get("temp").and_then(|v| v.as_f64()) {
                port_obj.insert("temp_c".into(), json!(temp));
                temp_max = Some(match temp_max {
                    Some(v) => v.max(temp),
                    None => temp,
                });
            }
            if let Some(vcc) = d.get("vcc").and_then(|v| v.as_f64()) {
                port_obj.insert("vcc_v".into(), json!(vcc));
            }
            if let Some(rx_arr) = d.get("rxPwr").and_then(|v| v.as_array()) {
                let rx_values: Vec<f64> = rx_arr.iter().filter_map(|x| x.as_f64()).collect();
                port_obj.insert("rx_power_mw".into(), json!(rx_values.clone()));
                for mw in &rx_values {
                    if *mw <= 0.0 {
                        continue; // no optic present
                    }
                    let dbm = 10.0_f64 * mw.log10();
                    rx_min = Some(match rx_min {
                        Some(v) => v.min(dbm),
                        None => dbm,
                    });
                }
            }
            if let Some(tx_arr) = d.get("txPwr").and_then(|v| v.as_array()) {
                let tx_values: Vec<f64> = tx_arr.iter().filter_map(|x| x.as_f64()).collect();
                port_obj.insert("tx_power_mw".into(), json!(tx_values));
            }
        }
        ports.push(Value::Object(port_obj));
    }

    (rx_min, temp_max, ports)
}
