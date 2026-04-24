// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Static registry of known Appear X-platform card-level JSON-RPC interfaces
//! that the discovery layer probes per slot at runtime.
//!
//! Each Appear card software (e.g. `net.appear.x5.hevc-sdi`,
//! `net.appear.x5.jpegxs-sdi`, the older IP-Gateway boards, the IP 2110
//! encoders, …) exposes its own set of versioned interfaces under the board
//! endpoint `https://{addr}/board/{slot:hex}/api/jsonrpc`. There is no
//! introspection method that lists what's installed, so the gateway probes a
//! known list at startup with cheap read-only Get calls and remembers which
//! `(interface, version, module, command)` quadruples this firmware actually
//! responds to.
//!
//! The list below is intentionally small and biased toward read-only Get
//! commands so probing is harmless on a live unit. Add new entries here as new
//! Appear card families are integrated.
//!
//! **Probe-param shapes.** Some Appear modules route the slot via the URL
//! path (`/board/<hex>/…`) and reject a `slot` key in the JSON-RPC params as
//! "Struct object has too many members"; others *require* `{"slot": N}` in
//! params (e.g. `Xger:2.55/cardStatus/GetCardStatus`). See [`ProbeParams`].

/// One probe candidate. The probe asks the unit for
/// `<interface>:<version>/<module>/<command>` with the `params` body and
/// considers the entry "available" on the first version that returns a
/// successful JSON-RPC result.
#[derive(Debug, Clone, Copy)]
pub struct ProbeEntry {
    /// Logical family name used in logs and reports (e.g. "Xger", "hipEnc",
    /// "ipGateway"). Not part of the wire protocol.
    pub family: &'static str,
    /// Wire interface name (e.g. "coderService", "ipGateway").
    pub interface: &'static str,
    /// Wire module name within the interface (e.g. "input", "coderService").
    pub module: &'static str,
    /// Read-only command to attempt during probing.
    pub command: &'static str,
    /// Versions to try, newest first. Probing stops at the first success.
    pub versions: &'static [&'static str],
    /// Shape of the `params` body for the probe — see [`ProbeParams`].
    pub params: ProbeParams,
}

/// Shape of the JSON-RPC `params` body to send with a probe. Picks between
/// the three conventions Appear cards actually use on the wire.
#[derive(Debug, Clone, Copy)]
pub enum ProbeParams {
    /// `{}` — most Get* commands.
    Empty,
    /// `{"query": {}}` — the cross-board services module (`board:*/services/*`).
    EmptyQuery,
    /// `{"slot": <n>}` — X5/X20 card-manager modules that expect the slot as a
    /// parameter *in addition to* the URL path (e.g. `Xger:2.55/cardStatus/*`).
    Slot,
}

/// All known card-level interfaces to probe per slot. Keep this list focused on
/// cheap read-only commands.
///
/// Versions are listed newest-first based on the API reference PDFs in
/// `Appear-X-Platform-API/` and live-probe evidence. When firmware exposes an
/// older version, probing will fall through until it finds one that responds.
pub const CARD_PROBES: &[ProbeEntry] = &[
    // ── Legacy IP Gateway boards (ME-3000 / ME-4000 family). The Integration
    //    Guide example shows ipGateway:1.15. Some firmwares jumped to 1.16+.
    ProbeEntry {
        family: "ipGateway",
        interface: "ipGateway",
        module: "input",
        command: "GetInputs",
        versions: &["1.20", "1.16", "1.15", "1.14", "1.10"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "ipGateway",
        interface: "ipGateway",
        module: "output",
        command: "GetOutputs",
        versions: &["1.20", "1.16", "1.15", "1.14", "1.10"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "ipGateway",
        interface: "ipGateway",
        module: "ipinterface",
        command: "GetIpInterfaces",
        versions: &["1.20", "1.16", "1.15", "1.14", "1.10"],
        params: ProbeParams::Empty,
    },
    // ── Cross-board services module (board:*/services/*). Lives under the
    //    `board` interface alongside ipGateway on ME-3000 / ME-4000 family
    //    cards. Uses `{query: {}}` params per the Appear X Platform API
    //    Guide's sample payloads.
    ProbeEntry {
        family: "board",
        interface: "board",
        module: "services",
        command: "GetInputServices",
        versions: &["2.16", "2.8", "4.1", "1.15"],
        params: ProbeParams::EmptyQuery,
    },
    // ── X5 / X10 / X20 card-manager surface (Xger interface). Covers chassis
    //    families where encoder/decoder services are driven through the
    //    shared card manager rather than a dedicated IP Gateway board. Live
    //    on firmware like `net.appear.x5.hevc-sdi` / `net.appear.x5.jpegxs-sdi`
    //    and on commissioned IP 2110 encoders.
    //
    //    `cardStatus/GetCardStatus` is the "is there anything alive here"
    //    probe — it needs `{"slot": N}` in params (slot-in-URL is not enough
    //    on X5 firmware). Everything else uses `{}` and the URL slot.
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "cardStatus",
        command: "GetCardStatus",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49", "2.44"],
        params: ProbeParams::Slot,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "cardAllocation",
        command: "GetCardAllocations",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "multiService",
        command: "GetMultiServices",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49", "2.16"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "audioProfile",
        command: "GetAudioProfiles",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49", "2.6"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "ipInterface",
        command: "GetIpInterfaces",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "imageUpload",
        command: "GetImages",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "poolConfig",
        command: "GetPoolConfig",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49"],
        params: ProbeParams::Empty,
    },
    // `coderService` / `ipConnection` / `lockStatus` are present on fully
    // commissioned Xger firmware (IP 2110 encoders with an encoder pool) but
    // not on uncommissioned X5 HEVC SDI units. Probing them lets the gateway
    // light up richer data when they're there without failing on a bare X5.
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "coderService",
        command: "GetCoderServices",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49", "2.44", "2.40"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "ipConnection",
        command: "GetIpConnections",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "lockStatus",
        command: "GetLockStatus",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "Xger",
        module: "psiStatus",
        command: "GetPsiStatus",
        versions: &["2.55", "2.54", "2.53", "2.52", "2.49"],
        params: ProbeParams::Empty,
    },
    // ── Pure-JPEG-XS encoder family (hipEnc reference). Modules use the
    //    `hip<Family><Module>` naming convention on their own interface. Kept
    //    for units with dedicated JPEG XS boards; ignored on X5 HEVC SDI.
    ProbeEntry {
        family: "hipEnc",
        interface: "hipEnc",
        module: "hipCardSettings",
        command: "GetCardSettings",
        versions: &["1.7", "1.5", "1.2", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipEnc",
        module: "hipEncoder",
        command: "GetEncoders",
        versions: &["1.7", "1.4", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipEnc",
        module: "hipEncStatus",
        command: "GetEncoderTransportStatus",
        versions: &["1.7", "1.6", "1.4", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipEnc",
        module: "hipIpInterface",
        command: "GetIpInterfaces",
        versions: &["1.7", "1.2", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipEnc",
        module: "hipNetworkStatus",
        command: "GetNetworkStatus",
        versions: &["1.7", "1.3", "1.0"],
        params: ProbeParams::Empty,
    },
    // ── HEVC-TS encoder family (hipTsEnc reference). Product-specific modules
    //    live on their own interface and are distinct from the Xger card
    //    manager. If a future X5 firmware exposes them, probing picks them up.
    ProbeEntry {
        family: "hipTsEnc",
        interface: "hipTsEnc",
        module: "hipTsEncoder",
        command: "GetEncoders",
        versions: &["1.11", "1.7", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipTsEnc",
        interface: "hipTsEnc",
        module: "hipEncStatus",
        command: "GetEncoderTransportStatus",
        versions: &["1.11", "1.6", "1.0"],
        params: ProbeParams::Empty,
    },
    // ── SDI physical-card family (sdi reference; lowercase modules).
    ProbeEntry {
        family: "sdi",
        interface: "sdi",
        module: "cardinfo",
        command: "GetCardInfo",
        versions: &["1.24", "1.23", "1.18", "1.2", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "sdi",
        interface: "sdi",
        module: "cardsettings",
        command: "GetCardSettings",
        versions: &["1.24", "1.23", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "sdi",
        interface: "sdi",
        module: "physicalports",
        command: "GetPhysicalPorts",
        versions: &["1.24", "1.23", "1.17"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "sdi",
        interface: "sdi",
        module: "portstatus",
        command: "GetPortStatus",
        versions: &["1.24", "1.23", "1.20"],
        params: ProbeParams::Empty,
    },
];
