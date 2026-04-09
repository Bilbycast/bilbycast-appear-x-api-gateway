// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

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
    pub inputs: BTreeMap<u32, Vec<Value>>,
    pub outputs: BTreeMap<u32, Vec<Value>>,
    pub ip_interfaces: BTreeMap<u32, Vec<Value>>,
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
        let mut initial = AppearXState::default();
        initial.status = "ok".to_string();
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

    /// Look up the discovered API version for an interface on a given slot.
    /// Returns `None` if the slot or interface was not discovered.
    pub fn discovered_version(&self, slot: u32, interface: &str) -> Option<String> {
        self.caps
            .slots
            .get(&slot)
            .and_then(|s| s.discovered_interfaces.get(interface))
            .map(|r| r.version.clone())
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
        let mut inputs_flat: Vec<Value> = Vec::new();
        for (slot, items) in &g.inputs {
            for item in items {
                let mut o = item.clone();
                if let Some(obj) = o.as_object_mut() {
                    obj.insert("slot".to_string(), json!(slot));
                }
                inputs_flat.push(o);
            }
        }
        let mut outputs_flat: Vec<Value> = Vec::new();
        for (slot, items) in &g.outputs {
            for item in items {
                let mut o = item.clone();
                if let Some(obj) = o.as_object_mut() {
                    obj.insert("slot".to_string(), json!(slot));
                }
                outputs_flat.push(o);
            }
        }
        let mut ifaces_flat: Vec<Value> = Vec::new();
        for (slot, items) in &g.ip_interfaces {
            for item in items {
                let mut o = item.clone();
                if let Some(obj) = o.as_object_mut() {
                    obj.insert("slot".to_string(), json!(slot));
                }
                ifaces_flat.push(o);
            }
        }

        // Slots from the static capability snapshot — board names, software
        // versions, and feature flags. Always present so the chassis card has
        // something to render even before the chassis_info poll lands.
        let slots: Vec<Value> = self
            .caps
            .slots
            .values()
            .map(|s| slot_to_json(s))
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
        })
    }
}

fn slot_to_json(s: &SlotCapabilities) -> Value {
    json!({
        "slot": s.slot,
        "name": s.name,
        "serial": s.serial,
        "software_id": s.software_id,
        "software_display_name": s.software_display_name,
        "software_version": s.software_version,
        "features": s.features,
    })
}
