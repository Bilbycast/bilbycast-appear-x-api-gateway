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
    /// Optional JSON params for the probe. `None` means `{}`. Some Get commands
    /// require a `query: {}` body.
    pub params: ProbeParams,
}

#[derive(Debug, Clone, Copy)]
pub enum ProbeParams {
    Empty,
    EmptyQuery,
}

/// All known card-level interfaces to probe per slot. Keep this list focused on
/// cheap read-only commands.
///
/// Versions are listed newest-first based on the API reference PDFs in
/// `Appear-X-Platform-API/`. When firmware exposes an older version, probing
/// will fall through until it finds one that responds.
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
    // ── IP 2110 encoder card family (Xger reference). The umbrella interface
    //    is `coderService` plus a series of status interfaces.
    ProbeEntry {
        family: "Xger",
        interface: "coderService",
        module: "coderService",
        command: "GetCoderServices",
        versions: &["2.44", "2.43", "2.42", "2.40", "2.36", "2.30", "2.20", "2.10"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "serviceStatus",
        module: "serviceStatus",
        command: "GetServiceStatus",
        versions: &["2.45", "2.40", "2.30", "2.20", "2.10", "2.0", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "cardConfig",
        module: "cardConfig",
        command: "GetCardConfig",
        versions: &["1.3", "1.2", "1.1", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "cardStatus",
        module: "cardStatus",
        command: "GetCardStatus",
        versions: &["1.9", "1.5", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "ipInterface",
        module: "ipInterface",
        command: "GetIpInterfaces",
        versions: &["1.6", "1.5", "1.4", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "ipConnection",
        module: "ipConnection",
        command: "GetIpConnections",
        versions: &["1.5", "1.4", "1.3", "1.2", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "Xger",
        interface: "multiService",
        module: "multiService",
        command: "GetMultiServices",
        versions: &["2.16", "2.10", "2.0", "1.0"],
        params: ProbeParams::Empty,
    },
    // ── JPEG XS encoder family (hipEnc / hipTsEnc references). Modules use
    //    the `hip<Family><Module>` naming convention.
    ProbeEntry {
        family: "hipEnc",
        interface: "hipCardSettings",
        module: "hipCardSettings",
        command: "GetCardSettings",
        versions: &["1.2", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipEncoder",
        module: "hipEncoder",
        command: "GetEncoders",
        versions: &["1.4", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipIpInterface",
        module: "hipIpInterface",
        command: "GetIpInterfaces",
        versions: &["1.2", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipNetworkStatus",
        module: "hipNetworkStatus",
        command: "GetNetworkStatus",
        versions: &["1.3", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "hipEnc",
        interface: "hipEncStatus",
        module: "hipEncStatus",
        command: "GetEncoderTransportStatus",
        versions: &["1.6", "1.4", "1.0"],
        params: ProbeParams::Empty,
    },
    // ── SDI JPEG XS Encoder TS family (sdi reference). Uses lowercase modules.
    ProbeEntry {
        family: "sdi",
        interface: "cardinfo",
        module: "cardinfo",
        command: "GetCardInfo",
        versions: &["1.2", "1.0"],
        params: ProbeParams::Empty,
    },
    ProbeEntry {
        family: "sdi",
        interface: "cardsettings",
        module: "cardsettings",
        command: "GetCardSettings",
        versions: &["1.0"],
        params: ProbeParams::Empty,
    },
];
