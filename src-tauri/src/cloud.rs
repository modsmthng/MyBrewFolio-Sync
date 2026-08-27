// SPDX-License-Identifier: GPL-3.0-or-later

use std::{sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use reqwest::{redirect::Policy, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::{
    credentials::CredentialStore,
    model::{DeviceRegistration, OAuthTokens, SyncObject},
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
    pub device_redirect_uri: String,
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
            device_redirect_uri: option_env!("MYBREWFOLIO_SYNC_DEVICE_CALLBACK_URL")
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "{}/v1/sync/device-auth/callback",
                        option_env!("MYBREWFOLIO_SYNC_API_URL")
                            .unwrap_or("https://mybrewfolio.com")
                            .trim_end_matches('/')
                    )
                }),
        }
    }
}

/// Every non-2xx response collapses into a deliberately vague user-facing
/// error. Record the HTTP status and a bounded server detail on stderr for
/// diagnostics; only failure bodies are logged, and they are truncated.
async fn log_http_failure(context: &str, response: Response) {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail: String = body.trim().chars().take(200).collect();
    eprintln!("{context}: HTTP {status} {detail}");
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PendingOAuth {
    pub verifier: String,
    pub state: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PendingDeviceAuthorization {
    pub oauth: PendingOAuth,
    pub request_id: String,
    pub poll_token: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuthorizationInfo {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceAuthorizationStart {
    request_id: String,
    user_code: String,
    verification_uri: String,
    poll_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceAuthorizationPoll {
    status: String,
    authorization_code: Option<String>,
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
    credentials: Arc<dyn CredentialStore>,
}

impl CloudClient {
    pub fn new(credentials: Arc<dyn CredentialStore>) -> Result<Self, CloudError> {
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| CloudError::Unreachable)?;
        Ok(Self {
            config: CloudConfig::bundled(),
            http,
            credentials,
        })
    }

    fn authorization_for_redirect(
        &self,
        redirect_uri: &str,
    ) -> Result<(Url, PendingOAuth), CloudError> {
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
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid offline_access")
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        Ok((url, PendingOAuth { verifier, state }))
    }

    pub fn authorization(&self) -> Result<(Url, PendingOAuth), CloudError> {
        self.authorization_for_redirect(&self.config.redirect_uri)
    }

    pub async fn begin_device_authorization(
        &self,
    ) -> Result<(DeviceAuthorizationInfo, PendingDeviceAuthorization), CloudError> {
        let (_url, oauth) = self.authorization_for_redirect(&self.config.device_redirect_uri)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(oauth.verifier.as_bytes()));
        let response = self
            .http
            .post(format!(
                "{}/v1/sync/device-auth/requests",
                self.config.api_url
            ))
            .json(&json!({ "state": oauth.state, "codeChallenge": challenge }))
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() {
            log_http_failure("device authorization request", response).await;
            return Err(CloudError::OAuth);
        }
        let started: DeviceAuthorizationStart =
            response.json().await.map_err(|_| CloudError::OAuth)?;
        Ok((
            DeviceAuthorizationInfo {
                user_code: started.user_code,
                verification_uri: started.verification_uri,
                expires_in: started.expires_in,
            },
            PendingDeviceAuthorization {
                oauth,
                request_id: started.request_id,
                poll_token: started.poll_token,
            },
        ))
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
        self.exchange_authorization_code(code, &pending, &self.config.redirect_uri)
            .await
    }

    async fn exchange_authorization_code(
        &self,
        code: &str,
        pending: &PendingOAuth,
        redirect_uri: &str,
    ) -> Result<(), CloudError> {
        let response = self
            .http
            .post(&self.config.token_url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", self.config.client_id.as_str()),
                ("redirect_uri", redirect_uri),
                ("code", code),
                ("code_verifier", pending.verifier.as_str()),
            ])
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if !response.status().is_success() {
            log_http_failure("authorization code exchange", response).await;
            return Err(CloudError::OAuth);
        }
        let token: TokenResponse = response.json().await.map_err(|_| CloudError::OAuth)?;
        self.credentials
            .save_tokens(&OAuthTokens {
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at: Utc::now().timestamp() + token.expires_in.unwrap_or(3600),
            })
            .map_err(|_| CloudError::OAuth)
    }

    pub async fn poll_device_authorization(
        &self,
        pending: &PendingDeviceAuthorization,
    ) -> Result<Option<()>, CloudError> {
        let response = self
            .http
            .post(format!(
                "{}/v1/sync/device-auth/requests/{}/poll",
                self.config.api_url, pending.request_id
            ))
            .json(&json!({ "pollToken": pending.poll_token }))
            .send()
            .await
            .map_err(|_| CloudError::Unreachable)?;
        if response.status() == StatusCode::ACCEPTED {
            return Ok(None);
        }
        if !response.status().is_success() {
            log_http_failure("device authorization poll", response).await;
            return Err(CloudError::OAuth);
        }
        let result: DeviceAuthorizationPoll =
            response.json().await.map_err(|_| CloudError::OAuth)?;
        if result.status != "authorized" {
            return Ok(None);
        }
        let code = result.authorization_code.ok_or(CloudError::OAuth)?;
        self.exchange_authorization_code(&code, &pending.oauth, &self.config.device_redirect_uri)
            .await?;
        Ok(Some(()))
    }

    async fn access_token(&self) -> Result<String, CloudError> {
        let mut tokens = self
            .credentials
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
            log_http_failure("token refresh", response).await;
            return Err(CloudError::Revoked);
        }
        let refreshed: TokenResponse = response.json().await.map_err(|_| CloudError::OAuth)?;
        tokens.access_token = refreshed.access_token;
        tokens.refresh_token = refreshed.refresh_token.or(Some(refresh));
        tokens.expires_at = Utc::now().timestamp() + refreshed.expires_in.unwrap_or(3600);
        self.credentials
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
            log_http_failure("device registration", response).await;
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
            log_http_failure("sync batch", response).await;
            return Err(CloudError::Rejected);
        }
        response.json().await.map_err(|_| CloudError::Rejected)
    }

    async fn device_json(
        &self,
        method: reqwest::Method,
        path: &str,
        device_id: &str,
        body: Value,
    ) -> Result<Value, CloudError> {
        let response = self
            .authorized(method, path)
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .json(&body)
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

    async fn device_get_json(&self, path: &str, device_id: &str) -> Result<Value, CloudError> {
        let response = self
            .authorized(reqwest::Method::GET, path)
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

    async fn device_empty(
        &self,
        method: reqwest::Method,
        path: &str,
        device_id: &str,
    ) -> Result<(), CloudError> {
        let response = self
            .authorized(method, path)
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
        Ok(())
    }

    pub async fn request_two_way_notes(&self, device_id: &str) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/notes/two-way/request",
            device_id,
            json!({}),
        )
        .await
    }

    pub async fn disable_two_way_notes(&self, device_id: &str) -> Result<(), CloudError> {
        self.device_empty(reqwest::Method::DELETE, "/v1/sync/notes/two-way", device_id)
            .await
    }

    pub async fn begin_notes_backup(
        &self,
        device_id: &str,
        slot: &str,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/notes/backups",
            device_id,
            json!({ "slot": slot }),
        )
        .await
    }

    pub async fn add_notes_backup_items(
        &self,
        device_id: &str,
        backup_id: &str,
        items: &[Value],
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            &format!("/v1/sync/notes/backups/{backup_id}/items"),
            device_id,
            json!({ "items": items }),
        )
        .await
    }

    pub async fn finalize_notes_backup(
        &self,
        device_id: &str,
        backup_id: &str,
        inventory_hash: &str,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            &format!("/v1/sync/notes/backups/{backup_id}/finalize"),
            device_id,
            json!({ "inventoryHash": inventory_hash }),
        )
        .await
    }

    pub async fn notes_activation_preview(
        &self,
        device_id: &str,
        backup_id: &str,
    ) -> Result<Value, CloudError> {
        self.device_get_json(
            &format!("/v1/sync/notes/activation-preview/{backup_id}"),
            device_id,
        )
        .await
    }

    pub async fn activate_two_way_notes(
        &self,
        device_id: &str,
        backup_id: &str,
        decisions: Value,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/notes/two-way/activate",
            device_id,
            json!({ "backupId": backup_id, "decisions": decisions }),
        )
        .await
    }

    pub async fn claim_outbound_notes(&self, device_id: &str) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/notes/outbound/claim",
            device_id,
            json!({}),
        )
        .await
    }

    pub async fn complete_outbound_note(
        &self,
        device_id: &str,
        operation_id: &str,
        result: Value,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            &format!("/v1/sync/notes/outbound/{operation_id}/result"),
            device_id,
            result,
        )
        .await
    }

    pub async fn notes_backup_items(
        &self,
        device_id: &str,
        backup_id: &str,
    ) -> Result<Value, CloudError> {
        self.device_get_json(
            &format!("/v1/sync/notes/backups/{backup_id}/items"),
            device_id,
        )
        .await
    }

    pub async fn apply_notes_restore_results(
        &self,
        device_id: &str,
        backup_id: &str,
        items: Value,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            &format!("/v1/sync/notes/backups/{backup_id}/restore-results"),
            device_id,
            json!({ "items": items }),
        )
        .await
    }

    pub async fn save_settings(
        &self,
        device_id: &str,
        duplicate_policy: &str,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::PUT,
            "/v1/sync/settings",
            device_id,
            json!({ "duplicatePolicy": duplicate_policy }),
        )
        .await
    }

    pub async fn resync_preview(
        &self,
        device_id: &str,
        inventory: Value,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/resync/preview",
            device_id,
            json!({ "items": inventory }),
        )
        .await
    }

    pub async fn resync_apply(
        &self,
        device_id: &str,
        decisions: Value,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/resync/apply",
            device_id,
            decisions,
        )
        .await
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::{CloudClient, CloudConfig, CredentialStore, OAuthTokens};
    use crate::store::StoreError;

    #[derive(Default)]
    struct TestCredentials {
        tokens: Mutex<Option<OAuthTokens>>,
        pending: Mutex<Option<String>>,
    }

    impl CredentialStore for TestCredentials {
        fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), StoreError> {
            *self
                .tokens
                .lock()
                .map_err(|_| StoreError::InvalidCredentials)? = Some(tokens.clone());
            Ok(())
        }

        fn tokens(&self) -> Result<Option<OAuthTokens>, StoreError> {
            Ok(self
                .tokens
                .lock()
                .map_err(|_| StoreError::InvalidCredentials)?
                .clone())
        }

        fn delete_tokens(&self) -> Result<(), StoreError> {
            *self
                .tokens
                .lock()
                .map_err(|_| StoreError::InvalidCredentials)? = None;
            Ok(())
        }

        fn save_pending_device_authorization(&self, value: &str) -> Result<(), StoreError> {
            *self
                .pending
                .lock()
                .map_err(|_| StoreError::InvalidCredentials)? = Some(value.into());
            Ok(())
        }

        fn pending_device_authorization(&self) -> Result<Option<String>, StoreError> {
            Ok(self
                .pending
                .lock()
                .map_err(|_| StoreError::InvalidCredentials)?
                .clone())
        }

        fn delete_pending_device_authorization(&self) -> Result<(), StoreError> {
            *self
                .pending
                .lock()
                .map_err(|_| StoreError::InvalidCredentials)? = None;
            Ok(())
        }
    }

    async fn response_server(responses: Vec<(&str, &str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let responses = responses
            .into_iter()
            .map(|(status, body)| (status.to_string(), body.to_string()))
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.expect("request accepted");
                let mut input = [0_u8; 8_192];
                let _ = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut input)).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response written");
            }
        });
        format!("http://{address}")
    }

    async fn contract_server(routes: Vec<(&str, &str, &str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_requests = observed.clone();
        let routes = routes
            .into_iter()
            .map(|(request, status, body)| {
                (request.to_string(), status.to_string(), body.to_string())
            })
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            for (expected, status, body) in routes {
                let (mut stream, _) = listener.accept().await.expect("request accepted");
                let mut input = [0_u8; 8_192];
                let length = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut input))
                    .await
                    .expect("request arrives")
                    .expect("request read");
                let request = String::from_utf8_lossy(&input[..length]).into_owned();
                let request_line = request.lines().next().unwrap_or_default().to_string();
                observed_requests
                    .lock()
                    .expect("observed requests")
                    .push(request);
                let (status, body) = if request_line == expected {
                    (status, body)
                } else {
                    ("500 Internal Server Error".to_string(), "{}".to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response written");
            }
        });
        (format!("http://{address}"), observed)
    }

    fn client(credentials: Arc<TestCredentials>, url: &str) -> CloudClient {
        let mut client = CloudClient::new(credentials).expect("cloud client");
        client.config = CloudConfig {
            api_url: url.into(),
            client_id: "test-client".into(),
            authorize_url: "https://login.example.test/authorize".into(),
            token_url: url.into(),
            redirect_uri: "mybrewfolio-sync://oauth/callback".into(),
            device_redirect_uri: "https://mybrewfolio.example.test/v1/sync/device-auth/callback"
                .into(),
        };
        client
    }

    #[test]
    fn authorization_uses_pkce_and_a_random_state() {
        let credentials = Arc::new(TestCredentials::default());
        let client = client(credentials, "http://127.0.0.1:1");

        let (url, pending) = client.authorization().expect("authorization URL");

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("login.example.test"));
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "code_challenge_method")
                .map(|(_, value)| value),
            Some("S256".into())
        );
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value),
            Some(pending.state.into())
        );
        assert!(!pending.verifier.is_empty());
    }

    #[tokio::test]
    async fn device_pairing_exchanges_the_one_time_code_without_server_token_storage() {
        let url = response_server(vec![
            ("201 Created", r#"{"requestId":"request-1","userCode":"ABCD-1234","verificationUri":"https://mybrewfolio.example.test/pair","pollToken":"poll-secret","expiresIn":600}"#),
            ("200 OK", r#"{"status":"authorized","authorizationCode":"one-time-code"}"#),
            ("200 OK", r#"{"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600}"#),
        ]).await;
        let credentials = Arc::new(TestCredentials::default());
        let client = client(credentials.clone(), &url);

        let (info, pending) = client
            .begin_device_authorization()
            .await
            .expect("pairing begins");
        assert_eq!(info.user_code, "ABCD-1234");
        assert_eq!(info.expires_in, 600);
        assert_eq!(
            client
                .poll_device_authorization(&pending)
                .await
                .expect("pairing polls"),
            Some(())
        );

        assert_eq!(
            credentials
                .tokens()
                .expect("tokens stored")
                .expect("tokens present")
                .access_token,
            "access-token"
        );
    }

    #[tokio::test]
    async fn expired_tokens_refresh_before_an_authorized_state_request() {
        let url = response_server(vec![
            (
                "200 OK",
                r#"{"access_token":"new-access","expires_in":3600}"#,
            ),
            (
                "200 OK",
                r#"{"items":[],"source":{"duplicatePolicy":"reuse_matching"}}"#,
            ),
        ])
        .await;
        let credentials = Arc::new(TestCredentials::default());
        credentials
            .save_tokens(&OAuthTokens {
                access_token: "expired-access".into(),
                refresh_token: Some("refresh-token".into()),
                expires_at: 0,
            })
            .expect("expired token stored");
        let client = client(credentials.clone(), &url);

        let state = client.state("device-1").await.expect("state loaded");

        assert_eq!(state["source"]["duplicatePolicy"], "reuse_matching");
        assert_eq!(
            credentials
                .tokens()
                .expect("tokens read")
                .expect("token present")
                .access_token,
            "new-access"
        );
    }

    #[tokio::test]
    async fn cloud_operations_use_the_documented_authenticated_endpoints() {
        let (url, observed) = contract_server(vec![
            (
                "POST /v1/sync/devices HTTP/1.1",
                "200 OK",
                r#"{"device":{"id":"device-1","sourceId":"source-1"}}"#,
            ),
            (
                "GET /v1/sync/state HTTP/1.1",
                "200 OK",
                r#"{"items":[],"source":{}}"#,
            ),
            (
                "POST /v1/sync/batches HTTP/1.1",
                "200 OK",
                r#"{"results":[{"index":0,"status":"created"}]}"#,
            ),
            (
                "POST /v1/sync/notes/two-way/request HTTP/1.1",
                "200 OK",
                "{}",
            ),
            ("DELETE /v1/sync/notes/two-way HTTP/1.1", "200 OK", "{}"),
            (
                "POST /v1/sync/notes/backups HTTP/1.1",
                "200 OK",
                r#"{"backup":{"id":"backup-1"}}"#,
            ),
            (
                "POST /v1/sync/notes/backups/backup-1/items HTTP/1.1",
                "200 OK",
                "{}",
            ),
            (
                "POST /v1/sync/notes/backups/backup-1/finalize HTTP/1.1",
                "200 OK",
                "{}",
            ),
            (
                "GET /v1/sync/notes/activation-preview/backup-1 HTTP/1.1",
                "200 OK",
                r#"{"items":[]}"#,
            ),
            (
                "POST /v1/sync/notes/two-way/activate HTTP/1.1",
                "200 OK",
                "{}",
            ),
            (
                "POST /v1/sync/notes/outbound/claim HTTP/1.1",
                "200 OK",
                r#"{"operations":[]}"#,
            ),
            (
                "POST /v1/sync/notes/outbound/op-1/result HTTP/1.1",
                "200 OK",
                "{}",
            ),
            (
                "GET /v1/sync/notes/backups/backup-1/items HTTP/1.1",
                "200 OK",
                r#"{"items":[]}"#,
            ),
            (
                "POST /v1/sync/notes/backups/backup-1/restore-results HTTP/1.1",
                "200 OK",
                r#"{"applied":1}"#,
            ),
            ("PUT /v1/sync/settings HTTP/1.1", "200 OK", "{}"),
            (
                "POST /v1/sync/resync/preview HTTP/1.1",
                "200 OK",
                r#"{"restoreItems":[]}"#,
            ),
            (
                "POST /v1/sync/resync/apply HTTP/1.1",
                "200 OK",
                r#"{"restored":0}"#,
            ),
            ("POST /v1/sync/heartbeat HTTP/1.1", "200 OK", "{}"),
            ("DELETE /v1/sync/devices/device-1 HTTP/1.1", "200 OK", "{}"),
        ])
        .await;
        let credentials = Arc::new(TestCredentials::default());
        credentials
            .save_tokens(&OAuthTokens {
                access_token: "valid-access".into(),
                refresh_token: Some("refresh".into()),
                expires_at: i64::MAX,
            })
            .expect("token stored");
        let client = client(credentials, &url);
        let item = crate::model::SyncObject {
            kind: "shot".into(),
            source_key: "1:2".into(),
            source_hash: "hash".into(),
            shot_source_key: None,
            data: json!({ "id": "1" }),
        };

        let registered = client
            .register_device("installation-1", "Test machine", "linux", "0.3.12")
            .await
            .expect("device registered");
        assert_eq!(registered.id, "device-1");
        client.state("device-1").await.expect("state loaded");
        client
            .batch("device-1", &[item])
            .await
            .expect("batch accepted");
        client
            .request_two_way_notes("device-1")
            .await
            .expect("two-way Notes requested");
        client
            .disable_two_way_notes("device-1")
            .await
            .expect("two-way Notes disabled");
        client
            .begin_notes_backup("device-1", "latest")
            .await
            .expect("backup started");
        client
            .add_notes_backup_items("device-1", "backup-1", &[json!({ "sourceKey": "1:2" })])
            .await
            .expect("backup items added");
        client
            .finalize_notes_backup("device-1", "backup-1", "inventory-hash")
            .await
            .expect("backup finalized");
        client
            .notes_activation_preview("device-1", "backup-1")
            .await
            .expect("activation preview loaded");
        client
            .activate_two_way_notes("device-1", "backup-1", json!({ "items": [] }))
            .await
            .expect("two-way Notes activated");
        client
            .claim_outbound_notes("device-1")
            .await
            .expect("outbound Notes claimed");
        client
            .complete_outbound_note("device-1", "op-1", json!({ "status": "applied" }))
            .await
            .expect("outbound Notes completed");
        client
            .notes_backup_items("device-1", "backup-1")
            .await
            .expect("backup items loaded");
        client
            .apply_notes_restore_results("device-1", "backup-1", json!([]))
            .await
            .expect("restore result applied");
        client
            .save_settings("device-1", "reuse_matching")
            .await
            .expect("settings saved");
        client
            .resync_preview("device-1", json!([]))
            .await
            .expect("resync preview loaded");
        client
            .resync_apply("device-1", json!({ "restoreItemIds": [] }))
            .await
            .expect("resync applied");
        client
            .heartbeat("device-1", true, Some("2026-08-27T10:00:00.000Z"), None)
            .await
            .expect("heartbeat sent");
        client.revoke("device-1").await.expect("device revoked");

        let requests = observed.lock().expect("observed requests");
        assert_eq!(requests.len(), 19);
        assert!(requests.iter().all(|request| request
            .contains("authorization: Bearer valid-access")
            || request.contains("Authorization: Bearer valid-access")));
        assert!(requests[0].contains("installationId"));
        assert!(requests[2].contains("sourceKey"));
        assert!(requests[13].contains("items"));
        assert!(requests[14].contains("duplicatePolicy"));
    }
}
