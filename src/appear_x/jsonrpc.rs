// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Proprietary

//! JSON-RPC 2.0 client for the Appear X platform API.

use anyhow::{bail, Context, Result};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::config::AppearXConfig;

/// JSON-RPC client for an Appear X unit.
#[derive(Clone)]
pub struct JsonRpcClient {
    http: reqwest::Client,
    base_url: String,
    username: String,
    password: String,
    token: Arc<RwLock<Option<String>>>,
    request_id: Arc<AtomicU64>,
}

impl JsonRpcClient {
    pub fn new(config: &AppearXConfig) -> Result<Self> {
        // TLS posture for the Appear X HTTPS endpoint. Three modes:
        //
        // 1. `cert_fingerprint` set → full CA-chain validation **and**
        //    leaf-cert SHA-256 must match the configured pin. This is
        //    the strongest mode and is the recommended posture for
        //    production deployments.
        // 2. `accept_self_signed_cert: true` (the default) → no TLS
        //    validation. Appear X chassis ship with self-signed certs
        //    out of the box and customer test rigs depend on this
        //    permissive default. Pin via #1 to upgrade.
        // 3. `accept_self_signed_cert: false` and no fingerprint →
        //    standard CA-chain validation against the system roots.
        let builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10));
        let builder = if let Some(ref fp) = config.cert_fingerprint {
            // The SDK's pinning path doesn't enforce the
            // BILBYCAST_ALLOW_INSECURE env var (the env-var guard is
            // only on the unconditional self-signed path). Pinning
            // gives stronger guarantees than self-signed acceptance
            // and is safe to enable without the guard.
            let tls_config = bilbycast_gateway_sdk::tls::build_tls_config(false, Some(fp))
                .map_err(|e| anyhow::anyhow!("Appear X TLS config: {e}"))?;
            tracing::info!(
                "Appear X TLS: certificate pinning enabled (fingerprint prefix: {}...)",
                &fp.chars().take(11).collect::<String>()
            );
            builder.use_preconfigured_tls(tls_config)
        } else {
            // Preserve historical behaviour exactly when no pin is
            // configured. `accept_self_signed_cert: true` (the default)
            // remains permissive without requiring an env var.
            builder.danger_accept_invalid_certs(config.accept_self_signed_cert)
        };
        let http = builder.build()?;

        Ok(Self {
            http,
            base_url: format!("https://{}", config.address),
            username: config.username.clone(),
            password: config.password.clone(),
            token: Arc::new(RwLock::new(None)),
            request_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Authenticate with the Appear X unit via BeginSession.
    pub async fn authenticate(&self) -> Result<()> {
        let url = format!("{}/mmi/api/jsonrpc", self.base_url);
        let id = self.next_id();

        let body = json!({
            "jsonrpc": "2.0",
            "method": "mmi:1.0/authentication/BeginSession",
            "params": {
                "local": {
                    "username": self.username,
                    "password": self.password,
                }
            },
            "id": id,
        });

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("BeginSession request failed")?;

        let result: serde_json::Value = resp.json().await?;

        if let Some(error) = result.get("error") {
            bail!(
                "BeginSession failed: {}",
                error
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
            );
        }

        let access_token = result
            .get("result")
            .and_then(|r| r.get("accessToken"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("BeginSession response missing accessToken"))?;

        *self.token.write().await = Some(access_token.to_string());
        debug!("Authenticated with Appear X unit");
        Ok(())
    }

    /// Call an RPC method on the MMI endpoint.
    pub async fn call_mmi(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/mmi/api/jsonrpc", self.base_url);
        self.call_rpc(&url, method, params).await
    }

    /// Call an RPC method on a board endpoint.
    pub async fn call_board(&self, slot: u32, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let slot_hex = format!("{:X}", slot);
        let url = format!("{}/board/{}/api/jsonrpc", self.base_url, slot_hex);
        self.call_rpc(&url, method, params).await
    }

    async fn call_rpc(
        &self,
        url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Ensure we have a token
        {
            let token = self.token.read().await;
            if token.is_none() {
                drop(token);
                self.authenticate().await?;
            }
        }

        let id = self.next_id();
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        let token = self.token.read().await.clone().unwrap_or_default();
        let resp = self
            .http
            .post(url)
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("RPC call to {} failed", method))?;

        let status = resp.status();
        let result: serde_json::Value = resp.json().await?;

        // Handle auth expiry — retry once with fresh token
        if status.as_u16() == 401
            || result
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_i64())
                .map(|c| c == -32600) // typical auth error code
                .unwrap_or(false)
        {
            warn!("Token expired, re-authenticating...");
            self.authenticate().await?;
            let token = self.token.read().await.clone().unwrap_or_default();
            let resp = self
                .http
                .post(url)
                .header("Authorization", format!("Bearer {}", token))
                .json(&body)
                .send()
                .await?;
            let result: serde_json::Value = resp.json().await?;
            return extract_result(result, method);
        }

        extract_result(result, method)
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::Relaxed)
    }
}

fn extract_result(response: serde_json::Value, method: &str) -> Result<serde_json::Value> {
    if let Some(error) = response.get("error") {
        let message = error
            .get("data")
            .and_then(|d| d.get("message"))
            .and_then(|m| m.as_str())
            .or_else(|| error.get("message").and_then(|m| m.as_str()))
            .unwrap_or("unknown error");
        bail!("RPC {} failed: {}", method, message);
    }

    Ok(response
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}
