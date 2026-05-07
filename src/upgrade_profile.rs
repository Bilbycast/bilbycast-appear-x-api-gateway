// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! Compile-time identity allowlist + [`UpgradeProfile`] for this gateway.
//!
//! Sigstore keyless verification compares the cert identity baked into
//! `manifest.sig.bundle` against [`ALLOWED_SIGNERS`] before this gateway
//! will trust any release. The list is kept tiny on purpose — exactly
//! one entry per `(repo, workflow path, ref pattern)` tuple that's
//! authorised to publish releases for this binary.
//!
//! When the release workflow filename changes (rename, fork, second
//! release lane) **add** a new entry here; never relax the regex on an
//! existing one. The list is the only thing standing between a
//! supply-chain-compromised CI workflow and a remote staged binary.

use bilbycast_gateway_sdk::upgrade::{AllowedSigner, UpgradeProfile};

/// Identity allowlist for `bilbycast-appear-x-api-gateway` upgrade
/// signatures. Mirrors the structure of
/// `bilbycast-edge/src/upgrade/trust.rs::ALLOWED_SIGNERS` but pinned
/// to this repo's own release workflow path.
pub const ALLOWED_SIGNERS: &[AllowedSigner] = &[
    AllowedSigner {
        issuer: "https://token.actions.githubusercontent.com",
        repo: "https://github.com/Bilbycast/bilbycast-appear-x-api-gateway",
        ref_pattern: "refs/tags/v*",
        workflow: "https://github.com/Bilbycast/bilbycast-appear-x-api-gateway/.github/workflows/nightly-release.yml",
    },
];

/// `UpgradeProfile` for this gateway. Threaded into
/// [`UpgradeCoordinator::new`](bilbycast_gateway_sdk::UpgradeCoordinator::new).
///
/// `device_type` must match the `device_type` in `manifest.json` — the
/// release workflow injects `appear_x` to match.
pub const PROFILE: UpgradeProfile = UpgradeProfile {
    repo: "Bilbycast/bilbycast-appear-x-api-gateway",
    binary_name: "bilbycast-appear-x-api-gateway",
    device_type: "appear_x",
    allowed_signers: ALLOWED_SIGNERS,
};
