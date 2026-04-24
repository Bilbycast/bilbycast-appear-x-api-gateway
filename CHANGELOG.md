# Changelog

All notable changes to `bilbycast-appear-x-api-gateway` are recorded here. The
format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Rebuilt the X5 / X10 / X20 monitoring and configuration surface around the
`Xger` card-manager interface after live-probing an X20_2RU + X5 HEVC SDI
unit revealed the existing `Xger:*/coderService/GetCoderServices` probe
does not respond on uncommissioned firmware. Broadcast engineers now see
PTP lock, SFP diagnostics, NMOS registration, and configuration entities
on every Appear chassis the gateway attaches to.

### Added
- **Xger probe registry**: 10 new entries covering `cardStatus`,
  `cardAllocation`, `multiService`, `audioProfile`, `ipInterface`,
  `imageUpload`, `poolConfig`, `coderService`, `ipConnection`, `lockStatus`,
  `psiStatus`. New `ProbeParams::Slot` variant for modules that expect
  `{"slot": N}` in params alongside the URL slot.
- **Per-slot Xger pollers** wired by capability discovery: `cardStatus`
  every 5 s (configurable via `polling.card_status_interval_secs`), all
  other Xger Get* calls every 30 s (`polling.xger_config_interval_secs`).
- **Synthetic events** emitted on edge-triggered cardStatus transitions:
  `ptp_lost` / `ptp_locked`, `sfp_low_rx_power` / `sfp_high_temperature`.
  Thresholds live in `polling.sfp_low_rx_dbm_threshold` (default −18 dBm)
  and `polling.sfp_high_temp_c_threshold` (default 70 °C).
- **Consolidated snapshot** now carries `card_status`, `coder_services`,
  `multi_services`, `audio_profiles`, `xger_ip_interfaces`,
  `card_allocations`, `pool_config`, `lock_status`, `psi_status`, and a
  derived `health_signals` block (per-slot + global rollup) for easy
  manager-side metric extraction.
- **Command surface** extended with the full Xger Get/Set symmetric set
  plus `clear_all_counters`: `get_card_status`, `get_coder_services`,
  `get_multi_services`, `get_audio_profiles`, `get_xger_ip_interfaces`,
  `get_card_allocations`, `get_pool_config`, `get_lock_status`,
  `get_psi_status`, `get_images`, `set_*` / `delete_*` pairs.
- **Enhanced `probe` subcommand**: after capability discovery, hits each
  discovered Xger module once and prints a broadcast-engineer-friendly
  per-slot summary (PTP state, per-port SFP temperature and Rx dBm,
  response sizes).
- Unit tests for `derive_health_signals` covering dark-optic rejection,
  worst-RX rollup, and partial-lock PTP rollup.

### Changed
- `SlotCapabilities.discovered_interfaces` → `discovered_modules`, keyed by
  `"<interface>/<module>"` so multiple modules on the same interface
  (every Xger module under `Xger`) are recorded independently.
- `SharedAppearXState::discovered_version(slot, interface)` →
  `discovered_version(slot, interface, module)`, plus a new
  `any_interface_version(slot, interface)` fallback.
- Alarm events now include `object_type` and `object_id` details from
  `configObjectType` / `configObjectId` for richer manager-side routing.

### Fixed
- On X5 HEVC SDI firmware, per-slot polling is no longer silent: the
  previous build discovered 0 modules and only reported chassis-level
  alarms. The new build discovers 7 Xger modules and polls all of them.
- `clear_all_counters` now routes to `hipEncStatus/ClearAllCounters` on
  the `hipTsEnc` (HEVC-TS) or `hipEnc` (JPEG-XS) interface, whichever
  this slot discovered. The previous build tried a non-existent
  `Xger:*/cardStatus/ClearAllCounters` method and always failed with
  "Method not found" — E2E probed against an X5 HEVC SDI unit showed
  the mis-routing. Slots with no encoder card family (bare X5 HEVC SDI)
  now return a clear `unsupported_on_card` error instead of a vendor
  API error.
- End-to-end `get_config` over the SDK no longer races: the SDK now
  emits `command_ack` **before** `config_response`, so the manager's
  unconditional cached-config invalidation on `command_ack` fires
  against an empty cache (no-op) and the subsequent `config_response`
  populates `cached_config` cleanly. Previously the ack's invalidation
  wiped the snapshot immediately after the response populated it, so
  `/api/v1/nodes/{id}/config` returned HTTP 404 on every first fetch
  until the moka 30 s TTL expired.
- Synthetic `sfp_high_temperature` / `sfp_low_rx_power` / `ptp_lost`
  events now surface with the correct severity in the manager. The
  gateway SDK's 4-level `EventSeverity` (Info/Minor/Major/Critical)
  was silently demoted to Info on the manager side — the manager's
  3-level enum's `from_str` did not recognise "minor" / "major".
  Manager-side fix collapses `"minor" → Warning` and `"major" →
  Critical`, so Appear X MINOR alarms and Minor synthetic events
  now paint as warnings and MAJOR alarms paint as critical (same
  colour class as CRITICAL).

## [0.4.0] - 2026-04-23

### Changed
- Adopted `bilbycast-gateway-sdk 0.1.0`. Hand-rolled WebSocket plumbing (~1160 lines) replaced with SDK calls. Vendor translation code (JSON-RPC to Appear X) is untouched.
- Reconnect backoff now exponential (1/2/5/10/30s) instead of fixed 5s.
- `command_ack.error_code` is now populated for every failure (previously absent on some paths).
- Auth frames now include `device_type: "appear_x"` (additive; manager ignores it).

### Removed
- `src/ws/` (client, envelope, TLS, auth) — moved into the SDK.
- `src/credentials.rs` — replaced by the SDK's `CredentialStore`.

### Unchanged
- Wire format byte-identical on every envelope.
- Vendor API translation logic unchanged.
- `config.toml` format unchanged.
- Probe mode (`cargo run -- probe`) unchanged.
- Event rate-limiter (950/min) still local; not yet in SDK.
