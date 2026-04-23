# Architecture

## Overview

The bilbycast-appear-x-api-gateway acts as a protocol bridge between two systems:

```
┌──────────────────┐     WebSocket (wss://)     ┌──────────────────────────────┐     JSON-RPC 2.0 (HTTPS)     ┌──────────────────┐
│                  │◄──────────────────────────►│  bilbycast-appear-x-api-gw   │◄──────────────────────────►│                  │
│ bilbycast-manager│  stats, health, commands   │                              │  GetInputs, SetOutputs,   │   Appear X Unit  │
│                  │  command_ack               │  ┌────────┐  ┌───────────┐  │  GetActiveAlarms, ...     │   (Chassis)       │
│  Dashboard, AI,  │                            │  │Polling │  │ Command   │  │                           │                  │
│  Topology        │                            │  │Engine  │  │ Handler   │  │                           │  Slot 1: IP GW   │
│                  │                            │  └────────┘  └───────────┘  │                           │  Slot 2: Encoder  │
└──────────────────┘                            │       │           │         │                           │  Slot N: ...      │
                                                │       └─────┬─────┘         │                           └──────────────────┘
                                                │             │               │
                                                │     ┌───────┴────────┐      │
                                                │     │ bilbycast-     │      │
                                                │     │ gateway-sdk    │      │
                                                │     │ (GatewayClient)│      │
                                                │     └────────────────┘      │
                                                └──────────────────────────────┘
```

The manager-facing WebSocket plumbing — TLS, auth, reconnect, heartbeat,
envelope serialisation, command dispatch — lives in the shared
[`bilbycast-gateway-sdk`](../../bilbycast-gateway-sdk/) crate. This gateway
is purely the vendor translation layer on top of that SDK.

## Data Flow

### Read Path (Polling → Manager)

1. Polling engine calls Appear X JSON-RPC methods on configured intervals
2. Responses are mapped to `stats` / `health` / `event` payloads
3. The polling engine calls `Emitter::emit_stats` / `emit_health` / `emit_event` on the SDK
4. The SDK's write task wraps each payload in the standard envelope and sends it on the WebSocket
5. Manager's `NodeHub` receives the message, updates cached stats, broadcasts to browser dashboards
6. The `AppearXDriver` in the manager extracts metrics for display

### Write Path (Manager → Appear X)

1. User clicks an action button in the AI assistant (or sends a command via API)
2. Manager sends a `command` envelope over WebSocket to the gateway
3. The SDK read loop dispatches it to `AppearXCommandHandler::handle_command` (or, for `get_config`, to `on_config_request`)
4. Command handler translates the action type to an Appear X JSON-RPC method
5. JSON-RPC call made to the Appear X unit
6. The handler returns `Result<Value, CommandError>`; the SDK serialises that into a `command_ack` (preserving `error_code`) and sends it back to the manager
7. Manager forwards the result to the UI

`CommandError::code` rides on `command_ack.error_code`. The gateway uses the
shared taxonomy: `validation_error`, `unknown_action`, `vendor_api_error`.

### Health Derivation

The gateway derives health status from Appear X alarms:

| Alarm Severity | Health Status |
|---------------|---------------|
| MAJOR or CRITICAL present | `critical` |
| MINOR or WARNING present | `degraded` |
| No alarms | `ok` |

## Security Model

### Manager Connection

The gateway implements the exact same security model as bilbycast-edge and bilbycast-relay — all of it inherited from [`bilbycast-gateway-sdk`](../../bilbycast-gateway-sdk/):

1. **TLS enforcement**: Only `wss://` connections accepted (`ws://` is rejected at config-validate time)
2. **Three TLS modes**:
   - **Standard**: Validates against system CA roots (webpki-roots)
   - **Self-signed**: Bypasses all cert validation (requires `BILBYCAST_ALLOW_INSECURE=1`)
   - **Pinned**: SHA-256 fingerprint verification of the server certificate
3. **Auth as first frame**: Credentials sent in the first WebSocket message, not in URL/headers
4. **Credential persistence**: After registration, node_id + node_secret saved to a file with 0600 permissions via the SDK's `CredentialStore`
5. **Reconnect backoff**: SDK-driven exponential backoff (1s → 2s → 5s → 10s → 30s, saturating), reset on successful auth
6. **Multi-URL failover**: the SDK rotates through `manager.urls[]` on every WS close

### Appear X Connection

- HTTPS with optional self-signed cert acceptance (separate from manager TLS)
- Bearer token authentication via JSON-RPC `BeginSession`
- Token auto-refresh on expiry (re-authenticates and retries the failed call)

## Concurrency Model

Two top-level tasks run concurrently via `tokio::spawn`:

```
main()
  ├── spawn: polling engine (multiple sub-tasks per board/poll type)
  └── await: GatewayClient::run (SDK — connect/auth/reconnect/read/write/heartbeat)
```

The SDK exposes:
- `Emitter` (cloneable) — the polling engine's outbound channel for stats / health / events / thumbnails / config_responses. Backed by `mpsc::channel<OutboundFrame>(256)` internally.
- `CommandHandler` trait — the SDK read loop dispatches every `command` envelope directly here; there is no longer a local mpsc+oneshot hop.
- `CancellationToken` — obtained via `client.shutdown_token()`, wired to ctrl-c in `main`.

Graceful shutdown uses `tokio_util::CancellationToken` — cancelling the token stops the SDK read/write/heartbeat tasks and signals the polling engine (which holds its own clone) to exit.

## Appear X API Details

### Endpoint Types

| Type | URL Pattern | Used For |
|------|------------|----------|
| MMI | `https://{addr}/mmi/api/jsonrpc` | Alarms, chassis model, authentication |
| Board | `https://{addr}/board/{slot_hex}/api/jsonrpc` | IP gateway, encoder, ASI per slot |
| Service | `https://{addr}/mmi/service_{name}/api/jsonrpc` | Cross-board services |

### Method Format

```
<interface>:<version>/<module>/<command>
```

Examples:
- `mmi:2.16/alarms/GetActiveAlarms`
- `ipGateway:1.15/input/GetInputs`
- `ipGateway:1.15/output/SetOutputs`
- `board:2.16/services/GetInputServices`

### Get/Set Symmetry

The Appear X API uses identical data structures for Get and Set operations. This means you can:
1. Call `GetInputs` to fetch current configuration
2. Modify the desired fields in the response
3. Call `SetInputs` with the modified data

The gateway's command handler leverages this: `set_ip_input` passes the `inputs` array directly to `SetInputs`.

### UUID Reference System

All entities in the Appear X platform are addressed by UUID:
- IP interfaces have UUIDs (referenced by inputs/outputs)
- Inputs have UUIDs (published as sources)
- Services within inputs have child UUIDs
- Outputs reference source UUIDs for content mapping

The AI assistant in the manager understands this reference system and can help users configure the correct UUID mappings.
