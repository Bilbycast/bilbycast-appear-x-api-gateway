# Adding New Device Gateways

This guide explains how to create a new API gateway for a 3rd-party broadcast device, using `bilbycast-appear-x-api-gateway` as the reference implementation.

## When to Use This Pattern

Use the API gateway pattern when integrating a device that:
- Has its own REST/JSON-RPC/SOAP API
- Cannot natively speak the bilbycast WebSocket protocol
- May be behind a firewall (the gateway connects outbound to the manager)
- Needs to appear in the bilbycast dashboard, topology, and AI assistant

## Two-Part Integration

Each 3rd-party device requires:

1. **API gateway binary** — a standalone Rust project that bridges the device's native API to the manager's WebSocket protocol
2. **Manager driver** — a `DeviceDriver` implementation in `bilbycast-manager` that defines metrics extraction, commands, and AI actions

## Step 1: Create the Gateway Project

### Project Structure

Copy the structure of `bilbycast-appear-x-api-gateway`:

```
bilbycast-<device>-api-gateway/
├── Cargo.toml
├── CLAUDE.md
├── src/
│   ├── main.rs              # CLI, config, tokio runtime
│   ├── config.rs            # TOML config parsing
│   ├── credentials.rs       # Node credential persistence (reuse as-is)
│   ├── ws/
│   │   ├── mod.rs
│   │   ├── client.rs        # WebSocket client (reuse as-is)
│   │   ├── tls.rs           # TLS config (reuse as-is)
│   │   └── message.rs       # WsEnvelope builders (reuse as-is)
│   └── <device>/
│       ├── mod.rs
│       ├── api_client.rs    # Device-specific API client
│       ├── polling.rs       # Polling engine (device-specific endpoints)
│       └── commands.rs      # Command handler (device-specific translation)
├── config/
│   └── example.toml
└── docs/
```

### Reusable Modules

The `ws/` directory can be copied directly — it implements the bilbycast WebSocket protocol and is device-agnostic:
- `ws/client.rs` — manager connection, auth, reconnect, message loop
- `ws/tls.rs` — three TLS modes matching edge/relay security
- `ws/message.rs` — WsEnvelope builders for stats, health, command_ack
- `credentials.rs` — node_id + node_secret persistence

When a second gateway exists, these should be extracted into a shared crate.

### Device-Specific Modules

Replace the `appear_x/` directory with your device's API integration:

**API Client** (`<device>/api_client.rs`):
- Handle device authentication (API keys, OAuth, session tokens, etc.)
- Implement request/response for the device's protocol (REST, SOAP, gRPC, etc.)
- Auto-retry on auth expiry

**Polling Engine** (`<device>/polling.rs`):
- Define what data to poll (status, config, alarms, metrics)
- Map device responses to `stats` and `health` messages
- Health derivation: map device health indicators to `ok`/`degraded`/`critical`

**Command Handler** (`<device>/commands.rs`):
- Map manager command types to device API calls
- Handle read commands (get current state) and write commands (apply config)
- Return success/error ack with optional response data

### Config Format

Define device-specific config sections in `config.rs`:

```toml
[manager]
# Same for all gateways — manager URL, auth, TLS settings
url = "wss://manager:8443/ws/node"
registration_token = "..."

[<device>]
# Device-specific connection settings
address = "192.168.1.x"
api_key = "..."

[polling]
# Device-specific polling intervals and targets
```

## Step 2: Create the Manager Driver

### Driver File

Create `bilbycast-manager/crates/manager-core/src/drivers/<device>.rs`:

```rust
use super::{
    ActionCategory, ActionUiHints, AiActionDescriptor, AiDeviceContext,
    CommandDescriptor, DeviceDriver, DeviceMetricsSummary,
};

pub struct MyDeviceDriver;

impl DeviceDriver for MyDeviceDriver {
    fn device_type(&self) -> &str { "<device>" }
    fn display_name(&self) -> &str { "My Device Name" }

    fn extract_metrics(&self, stats: &serde_json::Value) -> DeviceMetricsSummary {
        // Parse the stats JSON sent by your gateway's polling engine
    }

    fn extract_health_status(&self, health: &serde_json::Value) -> Option<String> {
        // Extract health from the health messages your gateway sends
    }

    fn supported_commands(&self) -> Vec<CommandDescriptor> {
        // List commands your gateway handles
    }

    fn validate_command(&self, action: &serde_json::Value) -> Result<(), String> {
        // Validate command payloads before sending to gateway
    }

    fn ai_context(&self) -> Option<AiDeviceContext> {
        // Provide protocol docs and config schema for the AI
    }

    fn ai_actions(&self) -> Vec<AiActionDescriptor> {
        // Define AI actions with prompt instructions and UI hints
        // All 3rd-party device actions should use execution_mode: "command"
    }
}
```

### AI Actions

When defining `ai_actions()`, use `execution_mode: "command"` for all actions. This routes through the generic `POST /nodes/{id}/command` endpoint, which the hub forwards via WebSocket to your gateway.

For `ConfigAction` category (complex payloads):
- Set `payload_key` to the JSON key holding the config (e.g., `"inputs"`, `"profile"`)
- Set `preview_type` to `"generic"` (renders a key-value card) or implement a custom preview

For `SimpleAction` category (buttons):
- Set appropriate `button_style`: `"info"` (blue), `"apply"` (green), `"delete"` (red), `"stop"` (orange)

### Register the Driver

Add to `bilbycast-manager/crates/manager-server/src/main.rs`:

```rust
driver_registry.register(Arc::new(manager_core::drivers::<device>::MyDeviceDriver::new()));
```

Add `pub mod <device>;` to `manager-core/src/drivers/mod.rs`.

## Step 3: Test End-to-End

1. Build and run the manager with the new driver registered
2. Create a node with `device_type: "<device>"` in the manager
3. Configure and run your gateway with the registration token
4. Verify:
   - Node appears online on dashboard
   - Stats populate from polling
   - Health status reflects device state
   - AI assistant shows device-specific actions
   - Commands execute through the full chain (UI → manager → WS → gateway → device → ack → UI)
