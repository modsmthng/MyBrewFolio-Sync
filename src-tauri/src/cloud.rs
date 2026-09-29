// SPDX-License-Identifier: GPL-3.0-or-later

use std::{sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use reqwest::{header::AUTHORIZATION, redirect::Policy, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;

mod auth;

use crate::{
    credentials::CredentialStore,
    model::{DeviceRegistration, OAuthTokens, SyncObject, SyncProgress},
};

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("MyBrewFolio Sync OAuth is not configured in this build")]
    NotConfigured,
    #[error("The MyBrewFolio connection could not be completed")]
    OAuth,
    #[error("The device authorization was rejected or has expired. Run auth begin again.")]
    DeviceAuthorizationRejected,
    #[error("The device authorization completed, but the OAuth code exchange failed. Run auth begin again.")]
    DeviceAuthorizationExchangeFailed,
    #[error("This Sync installation is no longer authorized")]
    Revoked,
    #[error("Your MyBrewFolio connection needs to be renewed")]
    ReauthRequired,
    #[error("MyBrewFolio could not verify this Sync installation")]
    AuthenticationRejected,
    #[error("MyBrewFolio could not be reached. Sync will retry automatically when the connection is available.")]
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
            redirect_uri: option_env!("MYBREWFOLIO_SYNC_REDIRECT_URI")
                .unwrap_or("mybrewfolio-sync://oauth/callback")
                .to_string(),
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

fn companion_capabilities() -> Value {
    json!({
        "profileStoreBridge": 2,
        "canonicalNotesHash": 1,
        "twoWayNotesProtocol": 2,
        "syncControl": 1,
        "syncSchedule": 1,
        "initialNotesActivation": 1,
        "syncProgress": 1,
        "multiMachine": 1,
    })
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
    refresh_lock: Mutex<()>,
    // Every engine belonging to one installation shares this client. Keep the
    // limit here so control actions and direct UI syncs count too.
    full_sync_slots: tokio::sync::Semaphore,
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
            refresh_lock: Mutex::new(()),
            full_sync_slots: tokio::sync::Semaphore::new(2),
        })
    }

    pub(crate) async fn full_sync_slot(&self) -> tokio::sync::SemaphorePermit<'_> {
        self.full_sync_slots
            .acquire()
            .await
            .expect("sync slots stay open")
    }

    pub async fn register_device(
        &self,
        installation_id: &str,
        name: &str,
        platform: &str,
        app_version: &str,
    ) -> Result<DeviceRegistration, CloudError> {
        let request = self
            .authorized(reqwest::Method::POST, "/v1/sync/devices")
            .await?
            .json(&json!({
                "installationId": installation_id,
                "name": name,
                "platform": platform,
                "appVersion": app_version,
                "capabilities": companion_capabilities()
            }));
        let response = self.send_authorized(request).await?;
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

    pub async fn register_installation(&self, installation_id: &str) -> Result<(), CloudError> {
        let request = self
            .authorized(reqwest::Method::POST, "/v1/sync/installations")
            .await?
            .json(&json!({
                "installationId": installation_id,
                "name": crate::engine::installation_name(),
                "platform": crate::engine::platform(),
                "appVersion": env!("CARGO_PKG_VERSION"),
                "capabilities": companion_capabilities(),
            }));
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            log_http_failure("installation registration", response).await;
            return Err(CloudError::Rejected);
        }
        Ok(())
    }

    pub async fn list_machines(&self, installation_id: &str) -> Result<Vec<Value>, CloudError> {
        let mut url = Url::parse(&format!("{}/v1/sync/machines", self.config.api_url))
            .map_err(|_| CloudError::NotConfigured)?;
        url.query_pairs_mut()
            .append_pair("installationId", installation_id);
        let request = self
            .authorized(
                reqwest::Method::GET,
                url.as_str().trim_start_matches(&self.config.api_url),
            )
            .await?;
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            log_http_failure("machine list", response).await;
            return Err(CloudError::Rejected);
        }
        let body: Value = response.json().await.map_err(|_| CloudError::Rejected)?;
        body.get("machines")
            .and_then(Value::as_array)
            .cloned()
            .ok_or(CloudError::Rejected)
    }

    pub async fn create_machine(
        &self,
        machine_id: &str,
        machine_name: &str,
        installation_id: &str,
    ) -> Result<(DeviceRegistration, bool), CloudError> {
        let request = self
            .authorized(reqwest::Method::POST, "/v1/sync/machines")
            .await?
            .json(&json!({
                "machineId": machine_id,
                "machineName": machine_name,
                "installationId": installation_id,
                "name": crate::engine::installation_name(),
                "platform": crate::engine::platform(),
                "appVersion": env!("CARGO_PKG_VERSION"),
                "capabilities": companion_capabilities(),
            }));
        let (device, body) = self
            .machine_device_response("create machine", request)
            .await?;
        let legacy_default = body
            .pointer("/machine/legacyDefault")
            .and_then(Value::as_bool)
            .ok_or(CloudError::Rejected)?;
        Ok((device, legacy_default))
    }

    pub async fn attach_machine(
        &self,
        machine_id: &str,
        installation_id: &str,
    ) -> Result<DeviceRegistration, CloudError> {
        let request = self
            .authorized(
                reqwest::Method::POST,
                &format!("/v1/sync/machines/{machine_id}/attach"),
            )
            .await?
            .json(&json!({
                "installationId": installation_id,
                "name": crate::engine::installation_name(),
                "platform": crate::engine::platform(),
                "appVersion": env!("CARGO_PKG_VERSION"),
                "capabilities": companion_capabilities(),
            }));
        self.machine_device_response("attach machine", request)
            .await
            .map(|(device, _)| device)
    }

    async fn machine_device_response(
        &self,
        context: &str,
        request: RequestBuilder,
    ) -> Result<(DeviceRegistration, Value), CloudError> {
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            log_http_failure(context, response).await;
            return Err(CloudError::Rejected);
        }
        let body: Value = response.json().await.map_err(|_| CloudError::Rejected)?;
        let device = body.get("device").ok_or(CloudError::Rejected)?;
        let registration = DeviceRegistration {
            id: device
                .get("id")
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?
                .to_string(),
            source_id: device
                .get("sourceId")
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?
                .to_string(),
        };
        Ok((registration, body))
    }

    pub async fn rename_machine(
        &self,
        machine_id: &str,
        installation_id: &str,
        name: &str,
    ) -> Result<(), CloudError> {
        let request = self
            .authorized(
                reqwest::Method::PATCH,
                &format!("/v1/sync/machines/{machine_id}"),
            )
            .await?
            .json(&json!({"installationId": installation_id, "name": name}));
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            log_http_failure("rename machine", response).await;
            return Err(CloudError::Rejected);
        }
        Ok(())
    }

    pub async fn detach_machine(
        &self,
        machine_id: &str,
        installation_id: &str,
    ) -> Result<(), CloudError> {
        let request = self
            .authorized(
                reqwest::Method::DELETE,
                &format!("/v1/sync/machines/{machine_id}/attachments/{installation_id}"),
            )
            .await?;
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            log_http_failure("detach machine", response).await;
            return Err(CloudError::Rejected);
        }
        Ok(())
    }

    pub async fn state(&self, device_id: &str) -> Result<Value, CloudError> {
        let request = self
            .authorized(reqwest::Method::GET, "/v1/sync/state")
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id);
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        response.json().await.map_err(|_| CloudError::Rejected)
    }

    pub async fn batch(&self, device_id: &str, items: &[SyncObject]) -> Result<Value, CloudError> {
        let request = self
            .authorized(reqwest::Method::POST, "/v1/sync/batches")
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .json(&json!({ "items": items }));
        let response = self.send_authorized(request).await?;
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
        let request = self
            .authorized(method, path)
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .json(&body);
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        response.json().await.map_err(|_| CloudError::Rejected)
    }

    async fn device_get_json(&self, path: &str, device_id: &str) -> Result<Value, CloudError> {
        let request = self
            .authorized(reqwest::Method::GET, path)
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id);
        let response = self.send_authorized(request).await?;
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
        let request = self
            .authorized(method, path)
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id);
        let response = self.send_authorized(request).await?;
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

    pub async fn create_notes_activation_from_import(
        &self,
        device_id: &str,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/notes/activation-from-import",
            device_id,
            json!({}),
        )
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
        sync_progress: Option<&SyncProgress>,
    ) -> Result<(), CloudError> {
        let request = self
            .authorized(reqwest::Method::POST, "/v1/sync/heartbeat")
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .json(&json!({
                "appVersion": env!("CARGO_PKG_VERSION"), "machineReachable": machine_reachable,
                "lastSyncAt": last_sync_at, "lastErrorCode": error,
                "syncProgress": sync_progress,
                "capabilities": companion_capabilities()
            }));
        let response = self.send_authorized(request).await?;
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        Ok(())
    }

    pub async fn claim_profile_store_operations(
        &self,
        device_id: &str,
        wait_seconds: u8,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            "/v1/sync/profile-store/operations/claim",
            device_id,
            json!({ "waitSeconds": wait_seconds.min(25) }),
        )
        .await
    }

    pub async fn complete_profile_store_operation(
        &self,
        device_id: &str,
        operation_id: &str,
        result: Value,
    ) -> Result<Value, CloudError> {
        self.device_json(
            reqwest::Method::POST,
            &format!("/v1/sync/profile-store/operations/{operation_id}/complete"),
            device_id,
            result,
        )
        .await
    }

    pub async fn control_request(
        &self,
        device_id: &str,
        path: &str,
        body: Value,
    ) -> Result<Value, CloudError> {
        let request = self
            .authorized(reqwest::Method::POST, &format!("/v1/sync/control/{path}"))
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id)
            .json(&body);
        let response = self.send_authorized(request).await?;
        // A dismissed or expired operation no longer needs a saved acknowledgement.
        if path.ends_with("/complete")
            && matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::CONFLICT
            )
        {
            return Ok(json!({"discarded":true}));
        }
        if !response.status().is_success() {
            return Err(CloudError::Rejected);
        }
        response.json().await.map_err(|_| CloudError::Rejected)
    }

    pub async fn revoke(&self, device_id: &str) -> Result<(), CloudError> {
        let request = self
            .authorized(
                reqwest::Method::DELETE,
                &format!("/v1/sync/devices/{device_id}"),
            )
            .await?
            .header("X-MyBrewFolio-Sync-Device", device_id);
        match self.send_authorized(request).await {
            Ok(response) if response.status().is_success() => Ok(()),
            Err(CloudError::Revoked) => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(CloudError::Rejected),
        }
    }
}

#[cfg(test)]
mod tests;
