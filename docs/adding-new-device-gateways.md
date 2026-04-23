# Adding New Device Gateways

This guide explains how to create a new API gateway for a 3rd-party broadcast device, using `bilbycast-appear-x-api-gateway` as the reference implementation.

Since Phase 6 of the plugin refactor, every manager-facing byte of the
WebSocket protocol lives in the shared [`bilbycast-gateway-sdk`](../../bilbycast-gateway-sdk/)
crate. **New gateways should consume the SDK — do NOT re-implement the
WebSocket client.** The SDK's own guide at
`bilbycast-gateway-sdk/docs/writing-a-gateway.md` is the authoritative
reference; this document focuses on the Appear X-specific patterns that
are worth copying.

## When to Use This Pattern

Use the API gateway pattern when integrating a device that:
- Has its own REST/JSON-RPC/SOAP API
- Cannot natively speak the bilbycast WebSocket protocol
- May be behind a firewall (the gateway connects outbound to the manager)
- Needs to appear in the bilbycast dashboard, topology, and AI assistant

## Two-Part Integration

Each 3rd-party device requires:

1. **API gateway binary** — a standalone Rust project that consumes
   `bilbycast-gateway-sdk` and implements `CommandHandler` + a polling loop
2. **Manager driver** — a `DeviceDriver` implementation in `bilbycast-manager` that defines metrics extraction, commands, and AI actions

## Step 1: Create the Gateway Project

### Project Structure

The canonical shape after Phase 6:

```
bilbycast-<device>-api-gateway/
├── Cargo.toml              # depends on bilbycast-gateway-sdk
├── CLAUDE.md
├── src/
│   ├── main.rs             # CLI, config, GatewayClient wiring, task spawning
│   ├── config.rs           # TOML config parsing
│   └── <device>/
│       ├── mod.rs
│       ├── api_client.rs   # Device-specific API client
│       ├── polling.rs      # Polling engine (device-specific endpoints)
│       └── commands.rs     # impl CommandHandler
├── config/
│   └── example.toml
└── docs/
```

No `ws/`, no `credentials.rs` — the SDK owns both.

### SDK-Provided Building Blocks

From `bilbycast-gateway-sdk`:

- `GatewayClient::connect(cfg, handler)` → `connect/reconnect/auth/heartbeat` loop
- `GatewayConfig` → operator-facing `[manager]` section shape (`manager_urls[]`, `registration_token`, credentials, TLS)
- `CredentialStore` → 0600 JSON persistence for `(node_id, node_secret)`
- `Emitter` → `emit_stats` / `emit_health` / `emit_event` / `emit_thumbnail` / `emit_config_response`
- `CommandHandler` trait → `handle_command(command_id, action)` + `on_config_request()`
- `CommandError::new("...", "...")` / `CommandError::unknown_action(...)` / `CommandError::validation(...)` — map onto `command_ack.error_code`
- `GatewayEvent::{info,minor,major,critical}(category, message).with_details(..).with_error_code(..)`

### Device-Specific Modules

**API Client** (`<device>/api_client.rs`):
- Handle device authentication (API keys, OAuth, session tokens, etc.)
- Implement request/response for the device's protocol (REST, SOAP, gRPC, etc.)
- Auto-retry on auth expiry

**Polling Engine** (`<device>/polling.rs`):
- Define what data to poll (status, config, alarms, metrics)
- Call `emitter.emit_stats(...)` / `emit_health(...)` / `emit_event(...)` on each poll
- Health derivation: map device health indicators to `ok`/`degraded`/`critical`

**Command Handler** (`<device>/commands.rs`):
- Implement `bilbycast_gateway_sdk::CommandHandler`
- Map `action["type"]` to device API calls
- Return `Result<Value, CommandError>` — the SDK packs this into `command_ack`
- Implement `on_config_request` to return the consolidated state snapshot

### Config Format

Define device-specific config sections in `config.rs`:

```toml
[manager]
# Shared across all gateways.
urls = ["wss://manager:8443/ws/node"]
registration_token = "..."
credentials_file = "credentials.json"
accept_self_signed_cert = false

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
