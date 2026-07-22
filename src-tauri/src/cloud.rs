// SPDX-License-Identifier: GPL-3.0-or-later

use std::{sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use reqwest::{redirect::Policy, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::{
    model::{DeviceRegistration, OAuthTokens, SyncObject},
    store::AppStore,
};

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("MyBrewFolio Sync OAuth is not configured in this build")]
    NotConfigured,
    #[error("The MyBrewFolio connection could not be completed")]
    OAuth,
    #[error("This Sync installation is no longer authorized")]
    Revoked,
    #[error("MyBrewFolio could not be reached")]
    Unreachable,
    #[error("MyBrewFolio rejected the synchronized data")]
    Rejected,
}

#[derive(Clone)]
pub struct CloudConfig {
    pub api_url: String,
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub redirect_uri: String,
}

impl CloudConfig {
    pub fn bundled() -> Self {
        Self {
            api_url: option_env!("MYBREWFOLIO_SYNC_API_URL")
                .unwrap_or("https://mybrewfolio.com")
                .trim_end_matches('/')
                .to_string(),
            client_id: option_env!("MYBREWFOLIO_SYNC_OAUTH_CLIENT_ID")
                .unwrap_or("")
                .to_string(),
            authorize_url: option_env!("MYBREWFOLIO_SYNC_AUTHORIZE_URL")
                .unwrap_or("https://clerk.mybrewfolio.com/oauth/authorize")
                .to_string(),
            token_url: option_env!("MYBREWFOLIO_SYNC_TOKEN_URL")
                .unwrap_or("https://clerk.mybrewfolio.com/oauth/token")
                .to_string(),
            redirect_uri: "mybrewfolio-sync://oauth/callback".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct PendingOAuth {
    pub verifier: String,
    pub state: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

pub struct CloudClient {
    pub config: CloudConfig,
    http: reqwest::Client,
    store: Arc<AppStore>,
}

impl CloudClient {
    pub fn new(store: Arc<AppStore>) -> Result<Self, CloudError> {
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| CloudError::Unreachable)?;
        Ok(Self {
            config: CloudConfig::bundled(),
            http,
            store,
        })
    }

    pub fn authorization(&self) -> Result<(Url, PendingOAuth), CloudError> {
        if self.config.client_id.is_empty() {
            return Err(CloudError::NotConfigured);
        }
        let mut verifier_bytes = [0_u8; 32];
        let mut state_bytes = [0_u8; 24];
        OsRng.fill_bytes(&mut verifier_bytes);
        OsRng.fill_bytes(&mut state_bytes);
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let state = URL_SAFE_NO_PAD.encode(state_bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url =
            Url::parse(&self.config.authorize_url).map_err(|_| CloudError::NotConfigured)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &self.config.redirect_uri)
            .append_pair("scope", "openid offline_access")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        Ok((url, PendingOAuth { verifier, state }))
    }

    pub async fn complete_authorization(
        &self,
        callback: &str,
        pending: PendingOAuth,
    ) -> Result<(), CloudError> {
        let url = Url::parse(callback).map_err(|_| CloudError::OAuth)?;
        if url.scheme() != "mybrewfolio-sync"
            || url.host_str() != Some("oauth")
            || url.path() != "/callback"
        {
            return Err(CloudError::OAuth);
        }
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        if parameters.get("state") != Some(&pending.state) || parameters.contains_key("error") {
            return Err(CloudError::OAuth);
        }
        let code = parameters.get("code").ok_or(CloudError::OAuth)?;
        let response = self
            .http
            .post(&self.config.token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", self.config.client_id.as_str()),
                ("redirect_uri", self.config.redirect_uri.as_str()),
                ("code", code.as_str()),
                ("code_verifier", pending.verifier.as_str()),
            ])
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() {
            return Err(CloudError::OAuth);
        }
        let token: TokenResponse = response.json().await.map_err(|_| CloudError::OAuth)?;
        self.store
            .save_tokens(&OAuthTokens {
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at: Utc::now().timestamp() + token.expires_in.unwrap_or(3600),
            })
            .map_err(|_| CloudError::OAuth)
    }

    async fn access_token(&self) -> Result<String, CloudError> {
        let mut tokens = self
            .store
            .tokens()
            .map_err(|_| CloudError::OAuth)?
            .ok_or(CloudError::Revoked)?;
        if tokens.expires_at > Utc::now().timestamp() + 60 {
            return Ok(tokens.access_token);
        }
        let refresh = tokens.refresh_token.clone().ok_or(CloudError::Revoked)?;
        let response = self
            .http
            .post(&self.config.token_url)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", self.config.client_id.as_str()),
                ("refresh_token", refresh.as_str()),
            ])
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() {
            return Err(CloudError::Revoked);
        }
        let refreshed: TokenResponse = response.json().await.map_err(|_| CloudError::OAuth)?;
        tokens.access_token = refreshed.access_token;
        tokens.refresh_token = refreshed.refresh_token.or(Some(refresh));
        tokens.expires_at = Utc::now().timestamp() + refreshed.expires_in.unwrap_or(3600);
        self.store
            .save_tokens(&tokens)
            .map_err(|_| CloudError::OAuth)?;
        Ok(tokens.access_token)
    }

    async fn authorized(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, CloudError> {
        let token = self.access_token().await?;
        Ok(self
            .http
            .request(method, format!("{}{}", self.config.api_url, path))
            .bearer_auth(token))
    }

    pub async fn register_device(
        &self,
        installation_id: &str,
        name: &str,
        platform: &str,
        app_version: &str,
    ) -> Result<DeviceRegistration, CloudError> {
        let response = self.authorized(reqwest::Method::POST, "/v1/sync/devices").await?
            .json(&json!({ "installationId": installation_id, "name": name, "platform": platform, "appVersion": app_version }))
            .send().await.map_err(|_| CloudError::Unreachable)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(CloudError::Revoked);
        }
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        let body: Value = response.json().await.map_err(|_| CloudError::Rejected)?;
        let device = body.get("device").ok_or(CloudError::Rejected)?;
        Ok(DeviceRegistration {
            id: device
                .get("id")
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?
                .to_string(),
            source_id: device
                .get("sourceId")
                .or_else(|| device.get("source_id"))
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?
                .to_string(),
        })
    }

    pub async fn state(&self, device_id: &str) -> Result<Value, CloudError> {
        let response = self
            .authorized(reqwest::Method::GET, "/v1/sync/state")
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(CloudError::Revoked);
        }
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        response.json().await.map_err(|_| CloudError::Rejected)
    }

    pub async fn batch(&self, device_id: &str, items: &[SyncObject]) -> Result<Value, CloudError> {
        let response = self
            .authorized(reqwest::Method::POST, "/v1/sync/batches")
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .json(&json!({ "items": items }))
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(CloudError::Revoked);
        }
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        response.json().await.map_err(|_| CloudError::Rejected)
    }

    pub async fn heartbeat(
        &self,
        device_id: &str,
        machine_reachable: bool,
        last_sync_at: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), CloudError> {
        let response = self
            .authorized(reqwest::Method::POST, "/v1/sync/heartbeat")
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .json(&json!({
                "appVersion": env!("CARGO_PKG_VERSION"), "machineReachable": machine_reachable,
                "lastSyncAt": last_sync_at, "lastErrorCode": error
            }))
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(CloudError::Revoked);
        }
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        Ok(())
    }

    pub async fn revoke(&self, device_id: &str) -> Result<(), CloudError> {
        let response = self
            .authorized(
                reqwest::Method::DELETE,
                &format!("/v1/sync/devices/{device_id}"),
            )
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() && response.status() != StatusCode::UNAUTHORIZED {
            return Err(CloudError::Rejected);
        }
        Ok(())
    }
}
