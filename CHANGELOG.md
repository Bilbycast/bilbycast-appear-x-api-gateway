# Changelog

All notable changes to `bilbycast-appear-x-api-gateway` are recorded here. The
format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.11.0] - 2026-05-08

### Added — Appear X redesign (phases A–G)

Major rework of the gateway and manager-side UI driven by an audit against
the live X5 chassis at 192.168.50.8 and the four refreshed PDF references
(Xger.2.58, ipGateway.1.57, mmi.5.6, timex.1.1, all extracted as `*.txt`
in `Appear-X-Platform-API/` for future grep). Scope decisions: multi-card
chassis target, same nav as today (chassis stays as a node under Managed
Nodes), forms-first (deprioritise AI-assisted authoring for Appear X),
no PCAP-download surface.

### Phase B — live telemetry on Overview

- New `ipGateway:*/status/Get*` pollers spawned per slot when discovery
  finds the `status` module: `GetIpInputStatus`, `GetIpOutputStatus`
  (with `GetOutputStatus` fallback on older firmware), `GetSrtInputStatus`,
  `GetSrtOutputStatus`. Same fast cadence as `cardStatus` (default 5 s).
- `ipGateway:*/physicalports/{GetPhysicalPorts,GetVirtualPorts}` slow
  pollers — port mode (SFP/RJ45), link rate, FEC, optical metrics, LACP /
  channel-bonding pairs.
- `ipGateway:*/triggers/GetTriggers` slow poller — per-card alarm
  trigger config snapshot.
- New stats fields: `ip_input_status`, `ip_output_status`,
  `srt_input_status`, `srt_output_status`, `phys_ports`, `virtual_ports`,
  `triggers`. Manager driver `extract_metrics` now surfaces
  `total_input_bps` / `total_output_bps` (parsed from string-encoded u64
  bitrates), `total_rtp_errors` / `total_cc_errors` /
  `total_sync_byte_errors` / `total_tei_errors`, and
  `active_srt_inputs` / `active_srt_outputs`.
- New on-demand commands: `get_ip_input_status`, `get_ip_output_status`,
  `get_srt_input_status`, `get_srt_output_status`, `get_pid_status`,
  `get_physical_ports`, `get_virtual_ports`, `get_triggers`.

### Phase C — Inputs/Outputs UX redesign (forms-first)

Transport-aware forms in `apx_form.js` for `ip_input` and `ip_output`:

- New framework primitives: `synthetic: true` field flag (rendered for
  visibility/discriminator duty but skipped in `collect()` so the wire
  JSON stays clean), and `derive(value) -> initial` field hook (compute
  initial value from existing entity body — needed to pre-populate the
  transport-type discriminator on edit).
- `ip_input` schema replaced with a transport-aware version covering
  UDP / RTP single, SRT caller, SRT listener, SRT rendezvous. Each
  branch's fields are `when:`-gated on the synthetic
  `_transport_type` discriminator. Fields cover address, port, RTP
  encapsulation, FEC, alarm-when-scrambled, RTP excessive-loss
  threshold (UDP); peer endpoint, local port, receive latency,
  high-bitrate mode, decryption algorithm + key (SRT). Analyze-mode
  field corrected to the canonical
  `analyzeMode.value.mpegMode.dejitter.value.bufferSize` path —
  the old `analyzeSettings.mode` flat enum was getting silently
  ignored by the chassis.
- `ip_output` schema similarly transport-aware, with the additional
  output-mode picker (raw / blacklistTs / whitelistTs / serviceMux)
  and TS-packets-per-IP-frame field for SRT outputs.
- Schema sources cited inline (`ipGateway:1.57 §46.2.x SrtCaller /
  SrtListener / SrtEncryption / SrtInputSettings / SrtOutputSettings`).

### Phase D — actionable alarms

- New on-demand commands: `get_alarm_history`
  (`mmi:*/alarms/GetAlarmsHistory` — accepts optional `since`,
  `limit`, `severityFilter` params; on-demand because the response can
  be large), `get_registered_alarms` (full trigger inventory), and
  `get_all_alarm_overrides`.
- All alarm payloads already carry `configObjectType`,
  `configObjectId`, `configObjectSlot`, `configObjectLabel`,
  `configObjectHandler` — sufficient for the manager UI to make alarm
  rows clickable to source object without a wire-protocol change.

### Phase E — alarm-override editor

- New commands: `set_alarm_overrides` (forwards
  `mmi:*/alarms/SetAlarmOverrides` — overrides change severity or
  hush specific triggers per alarm/object) and `delete_alarm_overrides`
  (`DeleteAlarmOverrides`).
- Per scope decision 2026-05-07: PCAP / network-capture surface
  deliberately NOT added — operators access PCAP via the chassis's
  own UI through the existing `web_ui_url` link.

### Phase F — TimeX + license surfaces (gated)

Per-slot probe entries for `TimeX:*/cardPtp` and
`TimeX:*/systemTimeSettings`; a probe miss leaves the slot's discovery
record without these modules and the manager UI hides the timing tab.
Bare X5 HEVC SDI 1.0.2 doesn't load TimeX — the surface lights up
on commissioned X10/X20 + 2110-encoder configurations. Slow per-slot
pollers for `cardPtp/Get{PtpStatus,PtpSettings}` and
`systemTimeSettings/GetSystemTimeStatus`. New stats fields
`ptp_status`, `ptp_settings`, `system_time_status`. Commands:
`get_ptp_status`, `get_ptp_settings`, `set_ptp_settings`,
`get_card_ptp_capabilities`, `get_system_time_status`,
`get_system_time_settings`, `set_system_time_settings`,
`get_current_utc_time`. `timex_call` helper mirrors `xger_call` —
returns `unsupported_on_card` instead of leaking a vendor "Method not
found" when the slot's discovery record doesn't include the module.

License surface (MMI module 15) added as on-demand only — operators
look at licensing infrequently and the chassis's polled state would
just duplicate the snapshot. Commands: `get_features_info` (installed
features + capacity + expiry), `get_license` (active blob),
`get_hardware_id`, `install_license`. Threaded through
`MmiVersions::chassis` (license is documented under the same envelope
as `chassisModel` in the X Platform 1.0.x reference).

### Fix — chronic alarms now stay visible on the manager events page

Bug: alarms emitted exactly one `alarm` event per transition (raise / clear).
A chassis with stable-but-broken state — the same six alarms active for
hours — would emit those six events once, then go silent for the rest of
the day. The manager's events page paginates by recent activity, so older
events scroll off and the chassis looks event-free even though it's still
broken.

Fix: `state.rs::set_alarms` now takes a `refresh_interval_secs` and
periodically force-clears `prev_alarm_ids` so the next diff treats every
currently-active alarm as new. New `[polling]` knob
`alarms_refresh_interval_secs` (default 1800 s = 30 min). Setting `0`
reverts to the legacy raise-once-only behaviour. Each refresh emits
N events where N is the active-alarm count — at the default cadence,
a 6-alarm chassis generates 12 alarm events per hour. The
`bilbycast_events_dropped_rate_limit_total` Prometheus counter on the
manager will catch any rate-limit pressure.

Manager UI: `events.html` category dropdown now lists `alarm`,
`target_reachability`, `ptp`, `sfp` so operators can filter to the
chassis-side noise. Category icons added (siren / satellite / bulb).

### Phase B+ — signal-flow diagram enrichment

The status-page signal canvas (`detail/appear_x_viz.js`) now consumes the
new live-telemetry surfaces and the chassis's own canonical signal-flow
tree. All additions gated on data presence — when the gateway hasn't
polled the new fields yet, or the firmware doesn't expose them, the
diagram falls back to the previous rendering byte-for-byte.

- **Live bitrates** decorate every input and output node sublabel
  ("UDP 10.0.0.10:6001 · 5.0 Mbps"). Sourced from `ip_input_status` /
  `ip_output_status` (`bitrates.value.effectiveFlowBitrate` parsed
  from string-encoded u64).
- **SRT peer state** decorates SRT input/output sublabels with peer
  endpoint + RTT ("peer 1.2.3.4:8000 / 35 ms"). Sourced from
  `srt_input_status` / `srt_output_status`.
- **Signal-present indicator** on input nodes — when the chassis
  publishes `input_services[].isPresent === false`, the input node
  goes amber instead of green. Real alarms still take precedence so a
  CRITICAL stays red.
- **New "Services" column** between Inputs and Coder Services. When
  `input_services` carries DvbSource / ServiceSource children inside an
  IP input proxy (the chassis's own canonical signal-flow tree, with
  service IDs, PMT/PCR PIDs, CC error counters), they render as their
  own nodes with edges from the parent input. Skipped when the tree is
  empty so older firmware degrades gracefully.
- **Pool coder services** merged with per-slot Xger coder services in
  the Coder column. Pool entries carry a `pool` tag in the sublabel so
  operators distinguish chassis-wide services from per-slot ones. Bare
  X5 HEVC SDI 1.0.2 has neither populated; commissioned X10/X20 chassis
  light up the column.
- **Video / audio profile resolution** — coder service nodes now show
  the real codec + resolution + frame rate ("HEVC 1920x1080 50fps")
  by joining `cv.video.profile.id` against `pool_video_profiles` /
  `audio_profiles`. The previous code read `cv.videoComponent.codec`
  directly; the new path falls back to the legacy shape when the new
  one isn't populated.
- **Edge inference extended** — coder→input edges now prefer routing
  through the discovered service when one matches the source UUID, so
  the chain reads "Input → Service → Coder → Output" on chassis that
  publish the service tree. Falls back to direct Input → Coder when
  no service matches.

### Phase G — schema-first form authoring

- `apx_form.js` schemas added for `ptp_settings` (PTP global +
  port settings — domain, profile, priority1/2, transport,
  delay mechanism, announce/sync/delay-request intervals) and
  `system_time_settings` (source priority, NTP servers as a
  repeatable group, hardware-clock timezone + UTC flag).
- The existing thin schemas (`hip_encoders`, `hip_decoders`, `dpi`,
  `esam_config`, `scte35_config`) remain functional and reasonable;
  the long-tail thinning continues in subsequent commits as new
  customer surfaces drive the need.

### Added — chassis uptime + freshness fixes

Phase A of the Appear X redesign: stop lying about chassis state.

- **`mmi:*/uptime/GetSystemUptime` poller**. New `[polling]` keys
  `uptime_mmi_version` (default `"5.6"`) and `uptime_interval_secs`
  (default `60`). Surfaces `chassis_uptime_secs` in every snapshot —
  distinct from the gateway sidecar's own `uptime_secs` (process age).
  Soft-fails on firmware that doesn't expose the `uptime` module
  (older variants); the field stays `null` and the manager UI hides
  the "Chassis up for …" line. Module is at version 1.0 internally
  but lives under the `mmi:5.6` envelope on current X Platform 1.0.x
  firmware.
- **Probe registry refreshed for current X Platform 1.0.x firmware**.
  `ipGateway` version list now leads with `1.57` and `1.39` (the
  module-internal `input(1.39)` shipping inside the `1.57` envelope
  on X5 HEVC SDI 1.0.2). `Xger` lists lead with `2.58`. Older
  versions kept at the tail for backward compatibility — discovery
  picks whichever the firmware actually answers, so the change is a
  pure addition.
- **`cards_mmi_versions` probe order** in `main.rs` flipped to
  newest-first (`["5.6", "5.0", "4.1", "4.0", "2.16", "2.8", "1.0"]`).
  The chassis is tolerant — most modern firmware accepts the entire
  range — but the version that wins gets threaded into every
  subsequent `mmi:*/cards/*` call, so picking the freshest one keeps
  us aligned with the firmware revision the operator is running.

### Changed — manager driver

- `extract_metrics` now surfaces three uptime fields:
  `gateway_uptime_secs` (sidecar process age), `chassis_uptime_secs`
  (Appear box uptime when the firmware exposes it, `null` otherwise),
  and the headline `uptime_secs` (chassis when present, gateway as
  fallback). Dashboard shows the chassis value because that's what an
  operator wants — "how long has the box been up" rather than "how
  long has my sidecar been running".
- `chassis_model` metric now joins the bare box type with the first
  populated slot's software display name + version. An X5 HEVC SDI
  1.0.2 chassis now reports `"X5 — X5 HEVC SDI 1.0.2-535993-79adb00e"`
  instead of just `"X5"`.

### Changed

- **Release workflow triggers** — `push.tags: v*` and `workflow_dispatch`
  added alongside the existing nightly cron. The new monorepo-root
  `release-all.sh` orchestrator detects Cargo.toml version bumps and pushes
  matching tags for on-demand releases; the nightly cron stays as a safety
  net. Workflow filename is preserved (`nightly-release.yml`) because it
  is hard-coded into `src/upgrade_profile.rs::ALLOWED_SIGNERS` and the
  cosign self-verify regex — renaming would lock every deployed gateway
  out of accepting new signed manifests. Manual install URL and the
  manager-driven auto-upgrade pipeline are unchanged.

### Added — remote upgrade

Remote, manager-driven binary upgrade — same Sigstore-keyless trust
chain the edge uses, parameterised through `bilbycast_gateway_sdk::upgrade`.
Operators can now ship a sidecar fix from the manager UI without SSHing
to every host running an Appear X chassis bridge.

- New `[upgrade]` TOML section (re-exports the SDK's `UpgradeConfig`
  shape) — `enabled`, `allowed_channels`, `install_root`, `min_version`,
  `boot_health_window_secs`, `max_boot_attempts`. Validation runs at
  load time for early operator feedback.
- New `src/upgrade_profile.rs` with the gateway's `ALLOWED_SIGNERS`
  identity allowlist and `UpgradeProfile` const pinning the repo +
  binary name + workflow path. The release workflow's path appears
  here verbatim — supply-chain compromise of any other CI workflow
  cannot stage a binary.
- `main.rs` wires the boot watchdog (runs before the WS connect so a
  crash-loop on the new binary triggers symlink revert), the
  coordinator, an event-forwarder task that drains
  `tokio::mpsc::Receiver<UpgradeEvent>` into the SDK Emitter, the
  periodic watchdog (promotes `pending_health → stable`), and a 15 s
  healthy-beat ticker.
- `appear_x/commands.rs::dispatch_upgrade_binary` arm — validates
  action shape, calls `coord.stage(...)`, on success schedules a
  5 s drain then `std::process::exit(0)` so systemd respawns into
  `current/`. Routed through both `DeferredAppearXHandler` (sidecar
  self-upgrade works pre-discovery) and `AppearXCommandHandler`
  (post-discovery) so a chassis-down event doesn't lock operators
  out of fixing a sidecar bug.
- `"upgrade"` capability advertised on every health envelope when
  `[upgrade]` is wired in TOML. The manager UI gates the per-node
  Upgrade button on this capability (the manager logic is
  device-type-agnostic; appear_x lights up automatically).
- New `packaging/` directory:
  - `install-appear-x-gateway.sh` — curl-pipe-bash installer with
    cosign verify-blob, manifest pinning, atomic symlink layout,
    `bilbycast-gateway` system user, systemd enable + health poll.
  - `uninstall-appear-x-gateway.sh` with optional `--purge-config` /
    `--purge-user`.
  - `bilbycast-appear-x-gateway.service` — hardened systemd unit
    (`ProtectSystem=strict`, no `CapabilityBoundingSet`, no `/dev`
    device allow-list — sidecars don't touch the GPU or audio).
  - `bilbycast-appear-x-gateway.sysusers` for `systemd-sysusers`.
- Release workflow rewritten to ship tarballs (binary + LICENSE +
  README + packaging/), generate `manifest.json` via the SDK's shared
  `scripts/build-manifest.sh`, sign with `cosign sign-blob --bundle`,
  paranoid self-verify against the production identity regex, and
  publish the manifest + bundle alongside the tarballs and standalone
  install scripts. Tag-push trigger added so maintainers can cut a
  release with `git tag vX.Y.Z`. `id-token: write` permission added
  for Sigstore Fulcio.

### Added — Xger card-manager surface

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
