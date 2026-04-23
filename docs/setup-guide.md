# Setup Guide

## Prerequisites

- A running bilbycast-manager instance (v0.4.5+)
- Network access from the gateway host to both the manager and the Appear X unit
- An Appear X unit with JSON-RPC API enabled (HTTPS)

## Step 1: Register the Node in Manager

1. Log into the bilbycast-manager web UI as an Admin
2. Navigate to **Managed Nodes** (`/admin/nodes`)
3. Click **Add Node**
4. Fill in:
   - **Name**: descriptive name (e.g., "Appear X - Studio A")
   - **Device Type**: select `appear_x` (Appear X Encoder/Gateway)
   - **Description**: optional
   - **Expiry**: optional
5. Click **Create** — a registration token is displayed
6. Copy the registration token

## Step 2: Configure the Gateway

1. Copy `config/example.toml` to your working directory as `config.toml`
2. Edit the configuration:

```toml
[manager]
urls = ["wss://your-manager-host:8443/ws/node"]
registration_token = "paste-your-token-here"
credentials_file = "credentials.json"

# For self-signed manager certs (dev/testing only):
# accept_self_signed_cert = true
# Requires: export BILBYCAST_ALLOW_INSECURE=1

# For production with cert pinning:
# cert_fingerprint = "ab:cd:ef:..."

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

3. No per-board entries are needed — the gateway runs a startup
capability-discovery pass and spawns per-slot pollers automatically
based on what each card's firmware actually exposes.

## Step 3: Run the Gateway

```bash
# Debug mode
cargo run -- --config config.toml

# Or with release build
cargo build --release
./target/release/bilbycast-appear-x-api-gateway --config config.toml
```

On first run, the gateway will:
1. Connect to the manager via WebSocket
2. Send the registration token
3. Receive node_id + node_secret credentials
4. Save credentials to `credentials.json`
5. Authenticate with the Appear X unit
6. Begin polling

On subsequent runs, it uses the saved credentials for reconnection (no token needed).

## Step 4: Verify in Manager

1. The node should appear as **online** on the manager dashboard
2. Stats (inputs, outputs, alarms) should populate within the configured polling intervals
3. Open the **AI Assistant** and select the Appear X node to ask questions or configure inputs/outputs

## Logging

Control log verbosity with `RUST_LOG`:

```bash
# Default (info level)
RUST_LOG=info cargo run -- --config config.toml

# Debug output for the gateway
RUST_LOG=info,bilbycast_appear_x_api_gateway=debug cargo run -- --config config.toml

# Trace-level for debugging JSON-RPC calls
RUST_LOG=info,bilbycast_appear_x_api_gateway=trace cargo run -- --config config.toml
```

## Security Notes

- The gateway enforces `wss://` for manager connections (no plaintext)
- Self-signed cert acceptance requires the explicit `BILBYCAST_ALLOW_INSECURE=1` env var
- For production, use certificate pinning (`cert_fingerprint`) instead of `accept_self_signed_cert`
- Credentials file is written with 0600 permissions (owner read/write only)
- The Appear X connection uses HTTPS; self-signed cert acceptance for the Appear X unit is independent of manager TLS settings

## Troubleshooting

| Symptom | Likely Cause | Fix |
|---------|-------------|-----|
| "Manager URL must use wss://" | Config has `ws://` URL | Change to `wss://` |
| "BILBYCAST_ALLOW_INSECURE=1 is not set" | Self-signed cert enabled without env var | `export BILBYCAST_ALLOW_INSECURE=1` |
| "Authentication failed" | Wrong token/credentials | Check registration token or re-register |
| "BeginSession failed" | Wrong Appear X credentials | Verify username/password in config |
| Node shows offline in manager | Network/firewall issue | Check connectivity to manager wss:// port |
| No stats appearing | Polling misconfigured | Check board slot numbers and API version |
