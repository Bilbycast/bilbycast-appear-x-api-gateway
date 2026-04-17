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
url = "wss://your-manager-host:8443/ws/node"
registration_token = "<token-from-manager>"

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
services_interval_secs = 30

[[polling.boards]]
slot = 1
interface = "ipGateway"
api_version = "1.15"
```

Add additional `[[polling.boards]]` entries for each board slot to monitor.

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
| `manager.url` | Yes | — | Manager WebSocket URL (must use `wss://`) |
| `manager.registration_token` | First run | — | One-time token from manager |
| `manager.credentials_file` | No | `credentials.json` | Where to persist node_id + node_secret |
| `manager.accept_self_signed_cert` | No | `false` | Accept self-signed certs (requires `BILBYCAST_ALLOW_INSECURE=1`) |
| `manager.cert_fingerprint` | No | — | SHA-256 fingerprint for certificate pinning |
| `appear_x.address` | Yes | — | Appear X unit IP or hostname |
| `appear_x.username` | Yes | — | JSON-RPC login username |
| `appear_x.password` | Yes | — | JSON-RPC login password |
| `appear_x.accept_self_signed_cert` | No | `true` | Accept Appear X self-signed HTTPS certs |

See `config/example.toml` for a complete template.

## CLI Options

```
bilbycast-appear-x-api-gateway [OPTIONS]

Options:
  -c, --config <PATH>    Path to TOML configuration file [default: config.toml]
  -h, --help             Print help
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

bilbycast-appear-x-api-gateway is **dual-licensed**:

- **AGPL-3.0-or-later** for open-source users — free for review, private use, and any use where you are comfortable releasing the source of your modifications (and any modified network service built on top of the gateway) under AGPL terms. See [LICENSE](LICENSE).
- **Commercial licence** from Softside Tech Pty Ltd for OEMs, hardware integrators, SaaS providers, and commercial customers who need to operate the gateway without AGPL § 13's source-release obligation. Contact **commercial@softsidetech.com** for terms. See [LICENSE.commercial](LICENSE.commercial).

Contributions are accepted under the Developer Certificate of Origin — see [DCO.md](DCO.md) and [CONTRIBUTING.md](CONTRIBUTING.md).
