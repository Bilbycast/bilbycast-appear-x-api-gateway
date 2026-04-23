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

Tests live alongside the sources (currently: `ws/event_gate.rs`
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

The gateway runs three concurrent tasks:
1. **WebSocket client** — connects to manager, handles auth, sends stats/health, receives commands
2. **Polling engine** — periodically calls Appear X JSON-RPC methods, maps responses to manager stats/health messages
3. **Command handler** — receives commands from manager via the WS client, translates them to Appear X JSON-RPC calls, returns ack

### Source Layout

```
src/
├── main.rs              # CLI (clap), config loading, tokio runtime, task spawning
├── config.rs            # TOML config parsing + validation
├── credentials.rs       # Node credential persistence (node_id + node_secret)
├── ws/
│   ├── mod.rs
│   ├── client.rs        # WebSocket client (auth, reconnect, message loop)
│   ├── tls.rs           # TLS config: standard CA, self-signed, cert pinning
│   └── message.rs       # WsEnvelope builders (stats, health, event, command_ack)
└── appear_x/
    ├── mod.rs
    ├── jsonrpc.rs        # JSON-RPC 2.0 client (session mgmt, request builder)
    ├── polling.rs        # Polling engine (alarms, chassis, inputs, outputs, services)
    └── commands.rs       # Command handler (manager commands → JSON-RPC calls)
```

### WebSocket Client (`ws/client.rs`)

Implements the same auth protocol as bilbycast-edge and bilbycast-relay:

- **wss:// enforcement** — rejects plaintext ws:// connections
- **Three TLS modes** (`ws/tls.rs`):
  - Standard: system CA roots via webpki-roots
  - Self-signed: requires `BILBYCAST_ALLOW_INSECURE=1` env var, logs security warning
  - Certificate pinning: SHA-256 fingerprint verification via `PinnedCertVerifier`
- **Auth as first frame** (not in URL):
  - Registration: `{"type": "auth", "payload": {"registration_token": "...", "software_version": "...", "protocol_version": 1}}`
  - Reconnection: `{"type": "auth", "payload": {"node_id": "...", "node_secret": "...", "software_version": "...", "protocol_version": 1}}`
- **10-second auth timeout**
- **Credential persistence** (`credentials.rs`): after registration, node_id + node_secret saved to JSON file with 0600 permissions
- **Exponential backoff reconnect**: 1s → 60s doubling, reset on success
- **Health heartbeat**: sent every 15 seconds
- **Command dispatch**: incoming `command` messages forwarded to command handler, `command_ack` sent back

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

| Poll Target | JSON-RPC Method | Endpoint | Manager Message |
|-------------|----------------|----------|-----------------|
| Alarms | `mmi:2.16/alarms/GetActiveAlarms` | MMI | `health` (derives status from alarm severity) |
| Chassis | `mmi:2.16/chassisModel/GetGraph` | MMI | `stats` |
| IP Inputs | `ipGateway:{ver}/input/GetInputs` | Board | `stats` |
| IP Outputs | `ipGateway:{ver}/output/GetOutputs` | Board | `stats` |
| Services | `board:{ver}/services/GetInputServices` | Board | `stats` |
| IP Interfaces | `ipGateway:{ver}/ipinterface/GetIpInterfaces` | Board | `stats` |

Health status derivation: `MAJOR`/`CRITICAL` alarms → "critical", `MINOR`/`WARNING` → "degraded", no alarms → "ok".

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

## Configuration

TOML config file. See `config/example.toml` for a complete template.

### Sections

| Section | Purpose |
|---------|---------|
| `[manager]` | Manager WebSocket URL, auth credentials/token, TLS settings |
| `[appear_x]` | Appear X unit address, login credentials, HTTPS settings |
| `[polling]` | Polling intervals per data type |
| `[[polling.boards]]` | Board slots to monitor (slot number, interface type, API version) |

### Key Settings

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `manager.url` | Yes | — | Manager WebSocket URL (must be `wss://`) |
| `manager.registration_token` | First run | — | One-time token from manager "Add Node" |
| `manager.credentials_file` | No | `credentials.json` | Where to persist node_id + node_secret |
| `manager.accept_self_signed_cert` | No | `false` | Accept self-signed certs (requires `BILBYCAST_ALLOW_INSECURE=1`) |
| `manager.cert_fingerprint` | No | — | SHA-256 fingerprint for cert pinning |
| `appear_x.address` | Yes | — | Appear X unit IP or hostname |
| `appear_x.username` | Yes | — | JSON-RPC login username |
| `appear_x.password` | Yes | — | JSON-RPC login password |
| `appear_x.accept_self_signed_cert` | No | `true` | Accept Appear X self-signed HTTPS certs |

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
  takes a list of up to 16 `wss://` URLs. The gateway client
  rotates through them on WS close with a fixed 5 s backoff —
  see `ws/client.rs`. Probe mode (`cargo run -- probe ...`) skips
  URL validation.
- **Per-node event rate limit (1000/min).** The manager drops
  excess events silently and synthesises one
  `event_rate_limit_exceeded` per window. The gateway runs its
  own `ws/event_gate.rs` at 950/min — strictly below the manager
  cap so client-side self-gating trips first. When it clamps it
  emits a single `event_rate_limit_selfgate` summary per window
  so the operator sees the exact suppressed count. No change to
  steady-state behaviour; the gate only matters during alarm
  storms.
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

## Appear X Platform Reference

The Appear X platform uses:
- **JSON-RPC 2.0** over HTTPS with Bearer token authentication
- **Modular chassis** with numbered slots holding hot-swappable boards
- **UUID-based source reference system** — inputs publish sources, processing modules transform, outputs consume
- **Symmetrical Get/Set** — GetInputs and SetInputs use identical data structures
- **Slot numbering** in hexadecimal for board endpoints (slot 10 = "A")

Key API modules used by this gateway:
- `mmi:2.16/alarms` — Active alarm monitoring
- `mmi:2.16/chassisModel` — Chassis graph (slots, boards, nodes, relations)
- `ipGateway:1.15/input` — IP input configuration (UDP, multicast, seamless, analyze modes)
- `ipGateway:1.15/output` — IP output configuration (raw, TS blacklist/whitelist, service multiplexing)
- `ipGateway:1.15/ipinterface` — IP interface configuration (physical ports, addressing)
- `board:2.16/services` — Input/output service reference system

## Creating Additional Device Gateways

To create a gateway for another 3rd-party device, use this project as a template:
1. Copy the project structure
2. Replace `appear_x/` with device-specific modules (API client, polling, command handler)
3. Update `config.rs` with device-specific settings
4. Create a matching driver in `bilbycast-manager/crates/manager-core/src/drivers/`
5. Register the driver in `bilbycast-manager/crates/manager-server/src/main.rs`

The `ws/` module (WebSocket client, TLS, message builders) can be reused as-is. When a second gateway exists, consider extracting `ws/` into a shared crate (`bilbycast-gateway-common`).
