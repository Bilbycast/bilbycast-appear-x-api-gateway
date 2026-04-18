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
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(config.accept_self_signed_cert)
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

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

    /// Refresh the current session token.
    pub async fn refresh_session(&self) -> Result<()> {
        let result = self.call_mmi("mmi:1.0/authentication/RefreshSession", json!({})).await?;
        if let Some(token) = result.get("accessToken").and_then(|t| t.as_str()) {
            *self.token.write().await = Some(token.to_string());
            debug!("Session refreshed");
        }
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

    /// Call an RPC method on a service endpoint.
    pub async fn call_service(&self, service_name: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let url = format!("{}/mmi/service_{}/api/jsonrpc", self.base_url, service_name);
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
