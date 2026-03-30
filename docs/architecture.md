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
                                                │      ┌──────┴──────┐        │
                                                │      │  WS Client  │        │
                                                │      └─────────────┘        │
                                                └──────────────────────────────┘
```

## Data Flow

### Read Path (Polling → Manager)

1. Polling engine calls Appear X JSON-RPC methods on configured intervals
2. Responses are mapped to `stats` or `health` messages
3. Messages sent through an mpsc channel to the WebSocket client
4. WS client wraps in `WsEnvelope` and sends to manager
5. Manager's `NodeHub` receives the message, updates cached stats, broadcasts to browser dashboards
6. The `AppearXDriver` in the manager extracts metrics for display

### Write Path (Manager → Appear X)

1. User clicks an action button in the AI assistant (or sends a command via API)
2. Manager sends a `command` message via WebSocket to the gateway
3. WS client receives the command, creates a oneshot channel for the ack
4. Command forwarded to the command handler via mpsc channel
5. Command handler translates the action type to an Appear X JSON-RPC method
6. JSON-RPC call made to the Appear X unit
7. Response wrapped in a `command_ack` and sent back through the oneshot channel
8. WS client sends the ack to the manager
9. Manager forwards the result to the UI

### Health Derivation

The gateway derives health status from Appear X alarms:

| Alarm Severity | Health Status |
|---------------|---------------|
| MAJOR or CRITICAL present | `critical` |
| MINOR or WARNING present | `degraded` |
| No alarms | `ok` |

## Security Model

### Manager Connection

The gateway implements the exact same security model as bilbycast-edge and bilbycast-relay:

1. **TLS enforcement**: Only `wss://` connections accepted
2. **Three TLS modes**:
   - **Standard**: Validates against system CA roots (webpki-roots)
   - **Self-signed**: Bypasses all cert validation (requires `BILBYCAST_ALLOW_INSECURE=1`)
   - **Pinned**: SHA-256 fingerprint verification of the server certificate
3. **Auth as first frame**: Credentials sent in the first WebSocket message, not in URL/headers
4. **Credential persistence**: After registration, node_id + node_secret saved to a file with 0600 permissions
5. **Exponential backoff**: 1s → 60s on connection failures, reset on success

### Appear X Connection

- HTTPS with optional self-signed cert acceptance (separate from manager TLS)
- Bearer token authentication via JSON-RPC `BeginSession`
- Token auto-refresh on expiry (re-authenticates and retries the failed call)

## Concurrency Model

All three main tasks run concurrently via `tokio::spawn`:

```
main()
  ├── spawn: polling engine (multiple sub-tasks per board/poll type)
  ├── spawn: command handler (receives from mpsc channel)
  └── await: WS client (blocks until cancellation)
```

Communication between tasks uses tokio channels:
- `mpsc::channel<Value>(64)` — polling → WS client (stats/health messages)
- `mpsc::channel<CommandMessage>(32)` — WS client → command handler (incoming commands)
- `oneshot::channel` per command — command handler → WS client (ack response)

Graceful shutdown uses `tokio_util::CancellationToken` tree — cancelling the root token propagates to all child tokens.

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
