# bilbycast-appear-x-api-gateway

> 🌐 Learn more at **[bilbycast.com](https://bilbycast.com)** — the official website for the Bilbycast broadcast media transport suite.

API gateway sidecar that bridges the [Appear X](https://www.appear.net/) broadcast encoder/gateway platform to bilbycast-manager. Connects to the manager as a WebSocket client (same protocol as edge/relay nodes) and communicates with the Appear X unit via its JSON-RPC 2.0 API over HTTPS.

This project serves as the **reference implementation** for integrating 3rd-party broadcast devices into the bilbycast ecosystem.

## Quick Start

### 1. Register in the manager

In the manager UI, go to **Managed Nodes**, click **Add Node**, select device type **appear_x**, and copy the registration token.

### 2. Configure

```bash
cp config/example.toml config.toml
```

Edit `config.toml`:

```toml
[manager]
urls = ["wss://your-manager-host:8443/ws/node"]
registration_token = "<token-from-manager>"
credentials_file = "credentials.json"

[appear_x]
address = "192.168.1.100"
username = "admin"
password = "your-password"
accept_self_signed_cert = true

[polling]
alarms_interval_secs = 10
chassis_interval_secs = 30
inputs_interval_secs = 15
outputs_interval_secs = 15
cards_interval_secs = 30
```

Per-board polling is derived automatically via the runtime capability
discovery pass — there is no `[[polling.boards]]` section. See
`config/example.toml` for the fully annotated template.

### 3. Build and run

```bash
cargo build --release
./target/release/bilbycast-appear-x-api-gateway --config config.toml
```

On first run, the gateway registers with the manager and saves credentials locally. On subsequent runs, it reconnects automatically using the saved credentials.

### 4. Verify

The node should appear as **online** in the manager dashboard. Stats (inputs, outputs, alarms) populate within the configured polling intervals.

## Configuration

| Setting | Required | Default | Description |
|---------|----------|---------|-------------|
| `manager.urls` | Yes | — | Ordered list of manager WebSocket URLs (1–16 entries, each `wss://`) |
| `manager.registration_token` | First run | — | One-time token from manager |
| `manager.credentials_file` | No | `credentials.json` | Where to persist node_id + node_secret |
| `manager.accept_self_signed_cert` | No | `false` | Accept self-signed certs (requires `BILBYCAST_ALLOW_INSECURE=1`) |
| `manager.cert_fingerprint` | No | — | SHA-256 fingerprint for certificate pinning |
| `appear_x.address` | Yes | — | Appear X unit IP or hostname |
| `appear_x.username` | Yes | — | JSON-RPC login username |
| `appear_x.password` | Yes | — | JSON-RPC login password |
| `appear_x.accept_self_signed_cert` | No | `true` | Accept Appear X self-signed HTTPS certs |

See `config/example.toml` for a complete template.

## CLI

```
bilbycast-appear-x-api-gateway [OPTIONS] [COMMAND]

Commands:
  run      Run the gateway (default — this is what you get with no command)
  probe    Connect to the Appear X unit only and exercise each polling call once

Options:
  -c, --config <PATH>    Path to TOML configuration file [default: config.toml]
  -h, --help             Print help
```

`--config` is global, so it applies to either command.

### `probe`

`probe` talks to the chassis and nothing else — it never opens a manager connection. It authenticates, fires the four chassis-wide MMI polls once each — alarms, chassis graph, `cards/GetChassisInfo`, `cards/GetCardStates` — with a PASS/FAIL line and a truncated response body per call, then runs per-slot capability discovery and prints what each card reports: chassis type, the negotiated `cards/*` MMI version, per-slot name/serial/software, and the modules the probe registry matched (naming `src/appear_x/probe_registry.rs` when a firmware's namespace is not registered yet). It finishes with a one-shot Xger health snapshot per slot — PTP lock, worst SFP RX optical power, max SFP temperature, plus an OK/ERR line per probed module. It is the fastest way to answer "are my credentials right and which interface versions does this firmware expose?".

Because it never reaches the manager, `probe` skips the manager-URL validation the gateway normally does at load time — so it works from a config file whose `manager.urls` is empty or not yet decided, before a manager exists.

```bash
bilbycast-appear-x-api-gateway --config config.toml probe
```

## Documentation

- [Setup Guide](docs/setup-guide.md) — step-by-step registration and configuration
- [Architecture](docs/architecture.md) — system design and component details
- [Adding New Device Gateways](docs/adding-new-device-gateways.md) — template for integrating other devices

## Security

- Manager connections enforce `wss://` (no plaintext WebSocket)
- Self-signed cert acceptance requires `BILBYCAST_ALLOW_INSECURE=1` env var as a safety guard
- Certificate pinning supported via `cert_fingerprint`
- Credentials file written with `0600` permissions
- Appear X HTTPS settings are independent of manager TLS settings

## Licensing

bilbycast-appear-x-api-gateway is **proprietary software**. Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.

Use of this software requires a separate written licence agreement with Softside Tech Pty Ltd. No rights — including use, copying, modification, or redistribution — are granted by the presence of this source code. See [LICENSE](LICENSE) for the full terms.

For licensing inquiries, contact **contact@bilbycast.com**.

This repository does not accept external contributions.
