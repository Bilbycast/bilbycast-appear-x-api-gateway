# Changelog

All notable changes to `bilbycast-appear-x-api-gateway` are recorded here. The
format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
