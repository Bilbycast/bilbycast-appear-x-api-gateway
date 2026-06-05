# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

bilbycast-appear-x-api-gateway is a sidecar binary that bridges the Appear X broadcast encoder/gateway platform to bilbycast-manager. It connects to the manager as a WebSocket client (same protocol as edge/relay nodes) and communicates with the Appear X unit via its JSON-RPC 2.0 API over HTTPS.

This project serves as the **reference implementation** for integrating 3rd-party broadcast devices into the bilbycast ecosystem. Future device gateways should follow the same structural pattern.

## Build & Run Commands

```bash
# Build (debug)
cargo build

# Build (release)
cargo build --release

# Run with config file
cargo run -- --config config.toml

# Check compilation
cargo check

# Lint
cargo clippy
```

Tests live alongside the sources (currently: `event_gate.rs`
covers the client-side rate gate). Run with `cargo test`.

## Architecture

### Component Overview

```
bilbycast-manager (WebSocket) ←──── bilbycast-appear-x-api-gateway ────→ Appear X Unit (JSON-RPC 2.0)
                                           │
                                    ┌──────┴──────┐
                                    │              │
                              Polling Engine   Command Handler
                              (reads stats)    (writes config)
```

The gateway runs two top-level tasks:
1. **SDK WebSocket client** ([`bilbycast-gateway-sdk`](../bilbycast-gateway-sdk/)) — owns the manager connection, auth, reconnect, heartbeat, envelope serialisation, and command dispatch
2. **Polling engine** — periodically calls Appear X JSON-RPC methods, pushes stats/health/events through the SDK's `Emitter`. The `CommandHandler` implementation (`AppearXCommandHandler`) is called directly by the SDK read loop.

### Source Layout

```
src/
├── main.rs              # CLI (clap), config loading, SDK wiring, task spawning
├── config.rs            # TOML config parsing + validation
├── event_gate.rs        # 950/min client-side event rate-limiter
├── upgrade_profile.rs   # Sigstore identity allowlist (repo + workflow) for self-upgrade
└── appear_x/
    ├── mod.rs
    ├── jsonrpc.rs        # JSON-RPC 2.0 client (session mgmt, request builder)
    ├── polling.rs        # Polling engine (alarms, chassis, inputs, outputs, services)
    ├── commands.rs       # AppearXCommandHandler (impl of SDK CommandHandler)
    ├── capabilities.rs   # Startup capability discovery
    ├── probe_registry.rs # Registry of per-card-family probe candidates
    ├── reachability.rs   # ReachabilityState — target up/down tracking for gateway_target
    └── state.rs          # SharedAppearXState — consolidated polling snapshot
```

### Manager-facing WS plumbing (`bilbycast-gateway-sdk`)

The gateway delegates every manager-facing byte to the shared SDK crate. The SDK owns:

- **wss:// enforcement** — rejects plaintext ws:// connections
- **Three TLS modes**:
  - Standard: system CA roots via webpki-roots
  - Self-signed: requires `BILBYCAST_ALLOW_INSECURE=1` env var, logs security warning
  - Certificate pinning: SHA-256 fingerprint verification
- **Auth as first frame** (not in URL):
  - Registration: `{"type": "auth", "payload": {"registration_token": "...", "software_version": "...", "device_type": "appear_x", "protocol_version": 1}}`
  - Reconnection: `{"type": "auth", "payload": {"node_id": "...", "node_secret": "...", "software_version": "...", "device_type": "appear_x", "protocol_version": 1}}`
- **10-second auth timeout**
- **Credential persistence** via `CredentialStore` — after registration, node_id + node_secret saved to JSON file with 0600 permissions. The gateway registers an `on_register` callback to trigger the save.
- **Reconnect backoff**: SDK default 1s → 2s → 5s → 10s → 30s (saturating), reset on successful auth
- **Multi-URL failover**: rotates through `manager.urls[]` on every WS close
- **Health heartbeat**: sent every 15 seconds
- **Command dispatch**: `command` envelopes dispatched into `AppearXCommandHandler::handle_command`; `get_config` is routed through `on_config_request` which returns the consolidated state snapshot and the SDK emits the `config_response` envelope + ack.

### Appear X JSON-RPC Client (`appear_x/jsonrpc.rs`)

Handles the Appear X JSON-RPC 2.0 specifics:

- **Session management**: `BeginSession` with username/password → Bearer token → auto-retry on token expiry
- **Request format**: `{"jsonrpc": "2.0", "id": <counter>, "method": "<interface>:<version>/<module>/<command>", "params": {...}}`
- **Endpoint routing**:
  - MMI: `https://{address}/mmi/api/jsonrpc` — alarms, chassis model, authentication
  - Board: `https://{address}/board/{slot_hex}/api/jsonrpc` — IP gateway, encoder, ASI (slot in hex)
  - Service: `https://{address}/mmi/service_{name}/api/jsonrpc` — cross-board services
- **HTTPS**: separate self-signed cert acceptance for the Appear X connection (independent of manager TLS)

### Polling Engine (`appear_x/polling.rs`)

Spawns periodic polling tasks per configured board slot:

All interface versions below are placeholders (`{ver}`): MMI versions
are config-driven (`[polling]` defaults — alarms `2.8`, chassisModel
`4.1`, cards `2.8`, uptime `5.6`); `ipGateway`/board versions are
**negotiated per slot** from the probe list in `probe_registry.rs`.

| Poll Target | JSON-RPC Method | Endpoint | Manager Message |
|-------------|----------------|----------|-----------------|
| Alarms | `mmi:{ver}/alarms/GetActiveAlarms` | MMI | `health` (derives status from alarm severity) |
| Chassis | `mmi:{ver}/chassisModel/GetGraph` | MMI | `stats` |
| IP Inputs | `ipGateway:{ver}/input/GetInputs` | Board | `stats` |
| IP Outputs | `ipGateway:{ver}/output/GetOutputs` | Board | `stats` |
| Services | `board:{ver}/services/GetInputServices` | Board | `stats` |
| IP Interfaces | `ipGateway:{ver}/ipinterface/GetIpInterfaces` | Board | `stats` |

Health status derivation: `MAJOR`/`CRITICAL` alarms → "critical", `MINOR`/`WARNING` → "degraded", no alarms → "ok".

**Target reachability sub-status (`gateway_target`).** Every health heartbeat also carries a typed `gateway_target` block (via the SDK's `Emitter::emit_health_with_target`) with `{reachable, target_address, gateway_host, gateway_egress_ip, last_successful_poll_unix, last_error_code, consecutive_failures}`. The alarms poller is the natural reachability heartbeat — its outcome (success / failure) drives `ReachabilityState` (`src/appear_x/reachability.rs`). When `consecutive_failures` crosses `[appear_x] reachability_failure_threshold` (default 2), `reachable` flips to `false` and the manager's dashboard renders the third "Target down" amber state. The polled chassis address comes from `[appear_x] address`; the gateway's own host / egress IP are detected at startup (via `/proc/sys/kernel/hostname` and a UDP-egress probe respectively). `last_error_code` is a fixed enum (`http_timeout` | `tcp_refused` | `tls_handshake` | `auth_rejected` | `rpc_protocol_error` | `other`) — verbose vendor errors stay in this gateway's local logs, never on the wire.

**Reachability events (`category: target_reachability`).** Edge-triggered, dwell-gated transitions fire two events: `target_unreachable` (Critical) when the chassis becomes unreachable AND the new state has been stable past `reachability_event_dwell_secs` (default 60 s — defeats slow-flap noise on degraded uplinks); `target_recovered` (Info) on the first success after a sustained unreachable streak. Single-fire per flip — flapping uplinks don't spam events.

### Command Handler (`appear_x/commands.rs`)

Translates manager commands into Appear X JSON-RPC calls:

| Manager Command | Appear X Method | Notes |
|----------------|-----------------|-------|
| `get_inputs` | `GetInputs` | Read-only |
| `get_outputs` | `GetOutputs` | Read-only |
| `get_services` | `GetInputServices` | Read-only, lists available sources |
| `get_alarms` | `GetActiveAlarms` | Read-only |
| `get_chassis` | `GetGraph` | Read-only |
| `get_ip_interfaces` | `GetIpInterfaces` | Read-only |
| `set_ip_input` | `SetInputs` | Write — requires `slot` and `inputs` fields |
| `set_ip_output` | `SetOutputs` | Write — requires `slot` and `outputs` fields |

Write commands follow the Appear X Get/Set symmetry pattern: the data structures for GetInputs and SetInputs are identical.

### "Open Device Web UI" launch (operator-configured URL)

The gateway no longer proxies the chassis's web admin UI through the WSS link. The manager exposes an "Open Device Web UI" `<a target="_blank" rel="noopener noreferrer">` on the Appear X node detail page when the operator sets `nodes.web_ui_url` from `/admin/nodes`. The button opens that URL directly in a new browser tab — neither the manager nor this sidecar are in the request path. Operators point the URL at whatever they have already set up to reach the chassis's admin UI from their browser (an SSH `-L` tunnel, a port-forward on the LAN, the LAN address itself, etc.). No sidecar config field, no capability advertisement.

## Configuration

TOML config file. See `config/example.toml` for a complete template.

### Sections

| Section | Purpose |
|---------|---------|
| `[manager]` | Manager WebSocket URL, auth credentials/token, TLS settings |
| `[appear_x]` | Appear X unit address, login credentials, HTTPS settings |
| `[polling]` | Polling intervals per data type, MMI interface versions, SFP thresholds |

> **No per-board configuration.** Boards/slots are **auto-discovered at
> runtime** by a startup capability-discovery pass
> (`src/appear_x/capabilities.rs`): it reads `cards/GetChassisInfo` for
> the chassis type and per-slot card details, then probes the registry
> in `src/appear_x/probe_registry.rs` to learn which JSON-RPC interface
> and version each populated slot speaks. There is no
> `[[polling.boards]]` section.

### Key Settings

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `manager.urls` | Yes | — | Ordered list of manager WebSocket URLs (1–16, each `wss://`) |
| `manager.registration_token` | First run | — | One-time token from manager "Add Node" |
| `manager.credentials_file` | No | `credentials.json` | Where to persist node_id + node_secret |
| `manager.accept_self_signed_cert` | No | `false` | Accept self-signed certs (requires `BILBYCAST_ALLOW_INSECURE=1`) |
| `manager.cert_fingerprint` | No | — | SHA-256 fingerprint for cert pinning |
| `appear_x.address` | Yes | — | Appear X unit IP or hostname |
| `appear_x.username` | Yes | — | JSON-RPC login username |
| `appear_x.password` | Yes | — | JSON-RPC login password |
| `appear_x.accept_self_signed_cert` | No | `true` | Accept Appear X self-signed HTTPS certs |
| `appear_x.reachability_failure_threshold` | No | `2` | Consecutive failed alarm polls before flipping `gateway_target.reachable` to `false` |
| `appear_x.reachability_event_dwell_secs` | No | `60` | Minimum dwell time (seconds) in the new reachability state before firing a `target_unreachable` / `target_recovered` event |

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `BILBYCAST_ALLOW_INSECURE` | Set to `"1"` to allow `accept_self_signed_cert` for manager connection |
| `RUST_LOG` | Log level control (e.g., `info,bilbycast_appear_x_api_gateway=debug`) |

## Manager-Side Requirements

The gateway requires a matching driver registered in bilbycast-manager:
- **Driver**: `manager-core/src/drivers/appear_x.rs` (`AppearXDriver`)
- **Device type**: `"appear_x"`
- **Registration**: Create a node in the manager with `device_type: "appear_x"`, copy the registration token to the gateway config

The driver provides:
- Metrics extraction from stats (alarms, inputs, outputs, slot counts)
- Health status derivation from alarm severity
- AI context (Appear X protocol docs, config schema, field rules)
- AI actions for the generic action rendering system

### Scale-out alignment with manager's Phase 1-5 changes

The gateway speaks `WS_PROTOCOL_VERSION = 1` and the manager has
not bumped that constant, so every manager-side scale-out change
is transparent on the wire. For completeness, the manager-side
deltas that matter for this sidecar:

- **manager_urls[]** (replaces scalar `manager.url`). Config now
  takes a list of up to 16 `wss://` URLs. The gateway delegates
  rotation and reconnect to `bilbycast_gateway_sdk::GatewayClient`;
  the SDK walks the list on every WS close with exponential backoff
  (1s → 2s → 5s → 10s → 30s, saturating, reset on successful auth).
  Probe mode (`cargo run -- probe ...`) skips URL validation.
- **Per-node event rate limit (1000/min).** The manager drops
  excess events silently and synthesises one
  `event_rate_limit_exceeded` per window. The gateway runs its
  own `event_gate.rs` at 950/min — strictly below the manager
  cap so client-side self-gating trips first. When it clamps it
  emits a single `event_rate_limit_selfgate` summary per window
  so the operator sees the exact suppressed count. No change to
  steady-state behaviour; the gate only matters during alarm
  storms. (The SDK does not currently expose a rate-limit helper;
  the gate stays in the gateway pending a future SDK release.)
- **Cross-instance config_fetch RPC** (Phase 2 tail). Added on
  the manager side for HA pairs. The gateway does not implement
  it — when a dashboard on manager A asks for config of a
  gateway owned by manager B, manager B serves the cached
  config directly. No gateway change.
- **Cross-region `region_latency` histogram** (Phase 4/5). The
  manager samples latency between its own instances, not
  between gateway and manager. No gateway change needed.
- **`/api/v1/metrics` endpoint** (Phase 5). Manager-only, gated
  by bearer token + IP allowlist.
- **Encrypted backup / HA lifecycle** (`init`, `backup`,
  `restore`, `promote`, `rejoin`, `upgrade`). Manager-only
  operator tooling; the gateway reconnects after the manager
  restarts as usual.

## Remote upgrade

The gateway accepts `upgrade_binary` WS commands from the manager and stages a Sigstore-verified release tarball, atomically swaps the `current` symlink under `/opt/bilbycast/appear-x-gateway/`, then drains and exits for systemd respawn. A boot watchdog guarantees a failed upgrade rolls back automatically. Mirrors the edge's upgrade pattern; the heavy lifting lives in `bilbycast-gateway-sdk::upgrade`, parameterised by [`src/upgrade_profile.rs`](src/upgrade_profile.rs).

- **Coordinator wiring**: `main.rs` builds an `Option<Arc<UpgradeCoordinator>>` from `cfg.upgrade`, runs `run_boot_watchdog` *before* the WS connect (so a crash-loop on the new binary triggers symlink revert + `exit(1)` on the (`max_boot_attempts` + 1)th boot), spawns an event-forwarder task that drains `mpsc::Receiver<UpgradeEvent>` onto the SDK `Emitter`, spawns the periodic watchdog (promotes `pending_health → stable` after `boot_health_window_secs`), and runs a 15 s healthy-beat ticker.
- **Command arm**: `appear_x::commands::dispatch_upgrade_binary` validates the action shape, calls `coord.stage(version, channel, target_arch?, variant?)`, and on success schedules a 5 s drain then `std::process::exit(0)` so systemd respawns into `current/`. **Both** the `DeferredAppearXHandler` (pre-discovery) and the `AppearXCommandHandler` (post-discovery) route the arm — sidecar self-upgrade must work even when the chassis is unreachable.
- **Capability advertisement**: `"upgrade"` is added to every health envelope's `capabilities` array unconditionally — the SDK upgrade module is always compiled in, mirroring the edge's baseline. The manager UI is device-type-agnostic and gates the per-node Upgrade button on this capability, so appear_x lights up by default. When the operator hasn't wired `[upgrade]` in `config.toml`, `dispatch_upgrade_binary` safely refuses with `upgrade_disabled` and a pointer at the missing config; the button stays visible so operators can discover the feature.
- **Identity allowlist**: [`src/upgrade_profile.rs`](src/upgrade_profile.rs) pins the repo + workflow path `Bilbycast/bilbycast-appear-x-api-gateway/.github/workflows/nightly-release.yml` against `refs/tags/v*`. Sigstore Fulcio cert identity must match — supply-chain compromise of any other CI workflow cannot stage a binary.
- **On-disk layout**: `/opt/bilbycast/appear-x-gateway/{current → versions/<v>/, versions/, state.json, config.toml, credentials.json}`. Service account `bilbycast-gateway` (NOT `bilbycast`, which is the edge's user — they coexist on the same host with separate filesystem permissions).
- **Operator install bundle**: [`packaging/install-appear-x-gateway.sh`](packaging/install-appear-x-gateway.sh) (curl-pipe-bash with cosign verify-blob), [`packaging/bilbycast-appear-x-gateway.service`](packaging/bilbycast-appear-x-gateway.service), [`packaging/bilbycast-appear-x-gateway.sysusers`](packaging/bilbycast-appear-x-gateway.sysusers), [`packaging/uninstall-appear-x-gateway.sh`](packaging/uninstall-appear-x-gateway.sh).
- **Release pipeline**: tarballs + cosign-signed `manifest.json` published by `.github/workflows/nightly-release.yml` (tag-push trigger on `vX.Y.Z`). Uses the SDK's shared `bilbycast-gateway-sdk/scripts/build-manifest.sh`. Self-verify step catches workflow-path / allowlist mismatch before publishing.
- **Vendor parity**: `bilbycast-gateway-template` and `bilbycast-gateway-sdk/docs/writing-a-gateway.md` §9a + §9b document the same wiring for new vendor sidecars.

## Appear X Platform Reference

The Appear X platform uses:
- **JSON-RPC 2.0** over HTTPS with Bearer token authentication
- **Modular chassis** with numbered slots holding hot-swappable boards
- **UUID-based source reference system** — inputs publish sources, processing modules transform, outputs consume
- **Symmetrical Get/Set** — GetInputs and SetInputs use identical data structures
- **Slot numbering** in hexadecimal for board endpoints (slot 10 = "A")

Key API modules used by this gateway (interface versions shown as
`{ver}` — MMI versions are config-driven, defaulting to alarms `2.8` /
chassisModel `4.1` / cards `2.8`; `ipGateway`/board versions are
negotiated per slot via `probe_registry.rs`):
- `mmi:{ver}/alarms` — Active alarm monitoring
- `mmi:{ver}/chassisModel` — Chassis graph (slots, boards, nodes, relations)
- `ipGateway:{ver}/input` — IP input configuration (UDP, multicast, seamless, analyze modes)
- `ipGateway:{ver}/output` — IP output configuration (raw, TS blacklist/whitelist, service multiplexing)
- `ipGateway:{ver}/ipinterface` — IP interface configuration (physical ports, addressing)
- `board:{ver}/services` — Input/output service reference system

## Creating Additional Device Gateways

To create a gateway for another 3rd-party device, depend on `bilbycast-gateway-sdk` (see `../bilbycast-gateway-sdk/docs/writing-a-gateway.md`) and:

1. Add `bilbycast-gateway-sdk = { path = "../bilbycast-gateway-sdk" }` to `Cargo.toml`
2. Build a `GatewayConfig` from your TOML `[manager]` section
3. Implement `bilbycast_gateway_sdk::CommandHandler` — this is your vendor translation layer
4. Spawn your polling task with the `Emitter` returned by `client.emitter()`
5. Create a matching driver in `bilbycast-manager/crates/manager-core/src/drivers/`
6. Register the driver in `bilbycast-manager/crates/manager-server/src/main.rs`

All WebSocket / TLS / auth / reconnect / heartbeat / command-ack wiring is owned by the SDK — the gateway only implements the vendor API client, polling loop, and `CommandHandler`.
