// Copyright (c) 2026 Reza Rahimi. All rights reserved.
// SPDX-License-Identifier: Elastic-2.0

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config::ManagerConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub node_id: Option<String>,
    pub node_secret: Option<String>,
    pub registration_token: Option<String>,
}

impl Credentials {
    /// Load credentials from the credentials file, or create default from config.
    pub fn load_or_default(config: &ManagerConfig) -> Result<Self> {
        let path = Path::new(&config.credentials_file);
        if path.exists() {
            let contents = std::fs::read_to_string(path)?;
            let creds: Credentials = serde_json::from_str(&contents)?;
            if creds.node_id.is_some() && creds.node_secret.is_some() {
                tracing::info!("Loaded credentials from {}", path.display());
                return Ok(creds);
            }
        }

        Ok(Credentials {
            node_id: None,
            node_secret: None,
            registration_token: config.registration_token.clone(),
        })
    }

    /// Save credentials to the credentials file after registration.
    pub fn save(&self, credentials_file: &str) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        let path = Path::new(credentials_file);

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(path, &json)?;

        // Set restrictive permissions (0600) on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        tracing::info!("Credentials saved to {}", path.display());
        Ok(())
    }

    /// Returns true if we have valid reconnection credentials.
    pub fn has_credentials(&self) -> bool {
        self.node_id.is_some() && self.node_secret.is_some()
    }
}
