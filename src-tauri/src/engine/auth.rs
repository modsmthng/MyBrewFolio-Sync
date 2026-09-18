use chrono::Utc;
use uuid::Uuid;

use crate::{
    cloud::{DeviceAuthorizationInfo, PendingDeviceAuthorization},
    store::StoreError,
};

use super::{
    installation_name, platform, EngineError, SyncEngine, TWO_WAY_NOTES_PROTOCOL_ANNOUNCED_SETTING,
    TWO_WAY_NOTES_PROTOCOL_VERSION,
};

impl SyncEngine {
    pub async fn begin_oauth(&self) -> Result<url::Url, EngineError> {
        let (url, pending) = self.cloud.authorization()?;
        *self.pending_oauth.lock().await = Some(pending);
        Ok(url)
    }

    pub async fn complete_oauth(&self, callback: &str) -> Result<(), EngineError> {
        let pending = self
            .pending_oauth
            .lock()
            .await
            .take()
            .ok_or(EngineError::OAuthState)?;
        self.cloud.complete_authorization(callback, pending).await?;
        self.register_connected_device().await
    }

    pub async fn begin_device_oauth(&self) -> Result<DeviceAuthorizationInfo, EngineError> {
        let _auth = self.pending_oauth.lock().await;
        let (info, pending) = self.cloud.begin_device_authorization().await?;
        self.store.remove_setting("headless_pairing_disabled")?;
        self.store.set_setting(
            "device_auth_expires",
            &(Utc::now().timestamp() + info.expires_in as i64).to_string(),
        )?;
        self.store
            .set_setting("device_auth_url", &info.verification_uri)?;
        let value = serde_json::to_string(&pending).map_err(|_| StoreError::InvalidCredentials)?;
        self.credentials.save_pending_device_authorization(&value)?;
        Ok(info)
    }

    pub async fn poll_device_oauth(&self) -> Result<bool, EngineError> {
        let _auth = self.pending_oauth.lock().await;
        let value = match self.credentials.pending_device_authorization()? {
            Some(value) => value,
            None if self.status().await.connected => return Ok(true),
            None => return Err(EngineError::OAuthState),
        };
        let pending = serde_json::from_str::<PendingDeviceAuthorization>(&value)
            .map_err(|_| StoreError::InvalidCredentials)?;
        if self
            .cloud
            .poll_device_authorization(&pending)
            .await?
            .is_some()
        {
            self.credentials.delete_pending_device_authorization()?;
            self.register_connected_device().await?;
            return Ok(true);
        }
        Ok(false)
    }

    #[cfg(feature = "headless")]
    pub async fn restart_rejected_device_oauth(
        &self,
    ) -> Result<DeviceAuthorizationInfo, EngineError> {
        let _auth = self.pending_oauth.lock().await;
        self.credentials.delete_pending_device_authorization()?;
        self.store.remove_setting("device_auth_expires")?;
        self.store.remove_setting("device_auth_url")?;
        drop(_auth);
        self.begin_device_oauth().await
    }

    async fn register_connected_device(&self) -> Result<(), EngineError> {
        let installation_id = self
            .store
            .setting("installation_id")?
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.store
            .set_setting("installation_id", &installation_id)?;
        let device = self
            .cloud
            .register_device(
                &installation_id,
                installation_name(),
                platform(),
                env!("CARGO_PKG_VERSION"),
            )
            .await?;
        self.store.set_setting("device_id", &device.id)?;
        self.store.set_setting("source_id", &device.source_id)?;
        self.store.set_setting(
            TWO_WAY_NOTES_PROTOCOL_ANNOUNCED_SETTING,
            TWO_WAY_NOTES_PROTOCOL_VERSION,
        )?;
        let mut status = self.status.write().await;
        status.connected = true;
        status.this_device_id = Some(device.id);
        status.last_error = None;
        status.last_error_code = None;
        status.last_error_at = None;
        Ok(())
    }
}
