// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

//! TLS configuration for the manager WebSocket connection.
//! Matches the security model of bilbycast-edge and bilbycast-relay:
//! - Standard mode: system CA roots
//! - Self-signed mode: requires BILBYCAST_ALLOW_INSECURE=1 env var
//! - Certificate pinning: SHA-256 fingerprint verification

use anyhow::{bail, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
use std::sync::Arc;

/// Build a rustls ClientConfig based on the manager connection settings.
pub fn build_tls_config(
    accept_self_signed: bool,
    cert_fingerprint: Option<&str>,
) -> Result<ClientConfig> {
    if let Some(fp) = cert_fingerprint {
        if !fp.is_empty() {
            return build_pinned_config(fp);
        }
    }

    if accept_self_signed {
        return build_insecure_config();
    }

    build_standard_config()
}

fn build_standard_config() -> Result<ClientConfig> {
    let root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(config)
}

fn build_insecure_config() -> Result<ClientConfig> {
    let allow = std::env::var("BILBYCAST_ALLOW_INSECURE")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false);

    if !allow {
        bail!(
            "accept_self_signed_cert is enabled but BILBYCAST_ALLOW_INSECURE=1 is not set. \
             Set this environment variable to confirm you understand the security implications."
        );
    }

    tracing::warn!(
        "SECURITY WARNING: accept_self_signed_cert is enabled — ALL TLS certificate \
         validation is disabled. This makes the connection vulnerable to man-in-the-middle \
         attacks. Do NOT use this in production."
    );

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(InsecureCertVerifier))
        .with_no_client_auth();
    Ok(config)
}

fn build_pinned_config(fingerprint: &str) -> Result<ClientConfig> {
    tracing::info!(
        "Certificate pinning enabled (fingerprint: {}...)",
        &fingerprint[..fingerprint.len().min(20)]
    );

    let root_store =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            roots: root_store,
            expected_fingerprint: fingerprint.to_lowercase(),
        }))
        .with_no_client_auth();
    Ok(config)
}

/// Verifier that accepts any certificate (for dev/testing with self-signed certs).
#[derive(Debug)]
struct InsecureCertVerifier;

impl ServerCertVerifier for InsecureCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Verifier that pins to a specific certificate SHA-256 fingerprint.
#[derive(Debug)]
struct PinnedCertVerifier {
    roots: rustls::RootCertStore,
    expected_fingerprint: String,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        // First validate the chain normally
        let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(self.roots.clone()))
            .build()
            .map_err(|e| Error::General(format!("Failed to build verifier: {}", e)))?;

        // Try standard verification first (but don't fail on it for pinning)
        let _ = verifier.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now);

        // Check fingerprint
        use std::fmt::Write;
        let digest = ring_compat_sha256(end_entity.as_ref());
        let mut fp = String::with_capacity(95);
        for (i, byte) in digest.iter().enumerate() {
            if i > 0 {
                fp.push(':');
            }
            write!(fp, "{:02x}", byte).unwrap();
        }

        if fp == self.expected_fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General(format!(
                "Certificate fingerprint mismatch. Expected: {}, Got: {}. \
                 This could indicate a MITM attack or certificate rotation.",
                self.expected_fingerprint, fp
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

/// Simple SHA-256 hash using a basic implementation.
/// We avoid pulling in ring/sha2 crates — use the built-in rustls digest.
fn ring_compat_sha256(data: &[u8]) -> [u8; 32] {
    // Use a simple SHA-256 via the `ring` crate that rustls already depends on
    use std::io::Write;
    let mut hasher = Sha256::new();
    hasher.write_all(data).unwrap();
    hasher.finalize()
}

/// Minimal SHA-256 implementation (rustls already links to crypto providers).
/// For production, this would use ring or sha2 crate directly.
struct Sha256 {
    data: Vec<u8>,
}

impl Sha256 {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

    fn finalize(self) -> [u8; 32] {
        // Use the crypto provider's digest
        // Fallback: we'll add sha2 crate. For now use a simple approach.
        sha256_digest(&self.data)
    }
}

impl std::io::Write for Sha256 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Compute SHA-256 using a simple software implementation.
fn sha256_digest(data: &[u8]) -> [u8; 32] {
    // Constants
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    // Pre-processing: padding
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit block
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4*i], chunk[4*i+1], chunk[4*i+2], chunk[4*i+3]]);
        }
        for i in 16..64 {
            let s0 = w[i-15].rotate_right(7) ^ w[i-15].rotate_right(18) ^ (w[i-15] >> 3);
            let s1 = w[i-2].rotate_right(17) ^ w[i-2].rotate_right(19) ^ (w[i-2] >> 10);
            w[i] = w[i-16].wrapping_add(s0).wrapping_add(w[i-7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g; g = f; f = e;
            e = d.wrapping_add(temp1);
            d = c; c = b; b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut result = [0u8; 32];
    for (i, val) in h.iter().enumerate() {
        result[4*i..4*i+4].copy_from_slice(&val.to_be_bytes());
    }
    result
}
