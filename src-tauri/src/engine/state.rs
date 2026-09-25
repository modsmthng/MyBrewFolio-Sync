use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{Mutex, MutexGuard, RwLock};

use crate::{
    cloud::{CloudClient, CloudError},
    credentials::CredentialStore,
    local::{normalize_host, GaggiMateClient},
    model::{AppStatus, NoteBackupSummary, SyncProgress},
    store::AppStore,
};

use super::{diagnostic_guidance, sync_progress_log_line, EngineError, SyncEngine};

impl SyncEngine {
    pub fn open(
        store: Arc<AppStore>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, EngineError> {
        let cloud = CloudClient::new(credentials.clone())?;
        let host = store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".to_string());
        let has_tokens = credentials.tokens()?.is_some();
        let mut device_id = store.setting("device_id")?;
        if !has_tokens && device_id.is_some() {
            // A missing keychain entry after an earlier connection cannot be
            // paired safely with queued data from a different future account.
            store.clear_account_data()?;
            store.set_setting("reconnect_required_reason", "SYNC_REAUTH_REQUIRED")?;
            device_id = None;
        }
        let connected = has_tokens && device_id.is_some();
        let reconnect_reason = store.setting("reconnect_required_reason")?;
        let reconnect_error = match reconnect_reason.as_deref() {
            Some("SYNC_REAUTH_REQUIRED") => {
                Some("Your MyBrewFolio connection needs to be renewed".into())
            }
            Some("SYNC_DEVICE_REVOKED") => {
                Some("This Sync installation is no longer authorized".into())
            }
            _ => None,
        };
        let notes_sync_intro_seen = store.setting("notes_sync_intro_seen")?.as_deref() == Some("1");
        let issues = store.failures().unwrap_or_default();
        Ok(Self {
            store,
            cloud,
            credentials,
            pending_oauth: Mutex::new(None),
            status: RwLock::new(AppStatus {
                connected,
                machine_host: host,
                machine_reachable: false,
                syncing: false,
                last_sync_at: None,
                last_error: reconnect_error,
                last_error_code: reconnect_reason,
                last_error_at: None,
                sync_progress: None,
                profiles: 0,
                shots: 0,
                notes: 0,
                conflicts: 0,
                suppressed: 0,
                initial_sync_configured: false,
                duplicate_policy: "reuse_matching".into(),
                notes_sync_status: "one_way".into(),
                notes_sync_target_device_id: None,
                notes_sync_writer_device_id: None,
                this_device_id: device_id,
                notes_sync_intro_seen,
                note_backups: Vec::new(),
                issues,
            }),
            control_lock: Mutex::new(()),
            sync_lock: Mutex::new(()),
            profile_store_lock: Mutex::new(()),
            schedule_changed: tokio::sync::Notify::new(),
        })
    }

    pub async fn status(&self) -> AppStatus {
        let mut status = self.status.read().await.clone();
        // Cancelled futures drop their locks even when they cannot clear the cached flag.
        status.syncing = self.control_lock.try_lock().is_err()
            || self.sync_lock.try_lock().is_err()
            || self.profile_store_lock.try_lock().is_err();
        status
    }

    pub(super) async fn report_sync_progress(
        &self,
        device_id: &str,
        progress: SyncProgress,
        publish: bool,
    ) -> Result<(), EngineError> {
        {
            let mut status = self.status.write().await;
            status.machine_reachable = true;
            status.sync_progress = Some(progress.clone());
        }
        eprintln!("{}", sync_progress_log_line(&progress));
        if publish {
            self.cloud
                .heartbeat(device_id, true, None, None, Some(&progress))
                .await?;
        }
        Ok(())
    }

    pub(super) async fn clear_sync_progress(&self) {
        self.status.write().await.sync_progress = None;
    }

    pub async fn pause_operations(
        &self,
    ) -> (MutexGuard<'_, ()>, MutexGuard<'_, ()>, MutexGuard<'_, ()>) {
        let control = self.control_lock.lock().await;
        let sync = self.sync_lock.lock().await;
        let machine = self.profile_store_lock.lock().await;
        (control, sync, machine)
    }

    pub async fn diagnose(&self) -> Result<Value, EngineError> {
        let status = self.status().await;
        let pending = self.store.pending_count()?;
        let failures = self.store.failure_count()?;
        let issues = self.store.failures()?;
        Ok(json!({
            "connection": {
                "connected": status.connected,
                "machineHost": status.machine_host,
                "machineReachable": status.machine_reachable,
                "syncing": status.syncing,
                "lastSyncAt": status.last_sync_at,
                "lastError": status.last_error,
                "lastErrorCode": status.last_error_code,
                "lastErrorAt": status.last_error_at,
                "syncProgress": status.sync_progress,
            },
            "issues": issues,
            "items": {
                "profiles": status.profiles,
                "shots": status.shots,
                "notes": status.notes,
                "conflicts": status.conflicts,
                "suppressed": status.suppressed,
            },
            "queue": {
                "pending": pending,
                "failures": failures,
            },
            "duplicatePolicy": status.duplicate_policy,
            "guidance": diagnostic_guidance(&status, pending, failures),
        }))
    }

    pub async fn set_host(&self, host: &str) -> Result<(), EngineError> {
        let host = normalize_host(host)?;
        self.store.set_setting("machine_host", &host)?;
        self.status.write().await.machine_host = host;
        Ok(())
    }

    pub fn hide_app_icon(&self) -> Result<bool, EngineError> {
        Ok(self.store.setting("hide_app_icon")?.as_deref() == Some("1"))
    }

    pub fn set_hide_app_icon(&self, hidden: bool) -> Result<(), EngineError> {
        self.store
            .set_setting("hide_app_icon", if hidden { "1" } else { "0" })?;
        Ok(())
    }

    pub async fn configure_sync(&self, reuse_matching: bool) -> Result<(), EngineError> {
        let device_id = self
            .store
            .setting("device_id")?
            .ok_or(CloudError::Revoked)?;
        let policy = if reuse_matching {
            "reuse_matching"
        } else {
            "import_all"
        };
        self.cloud.save_settings(&device_id, policy).await?;
        let state = self.cloud.state(&device_id).await?;
        self.store.set_setting("cloud_state", &state.to_string())?;
        self.update_from_cloud_state(&state).await;
        Ok(())
    }

    pub async fn retry_failures(&self) -> Result<(), EngineError> {
        self.store.retry_failures()?;
        Ok(())
    }

    pub async fn dismiss_notes_sync_intro(&self) -> Result<(), EngineError> {
        self.store.set_setting("notes_sync_intro_seen", "1")?;
        self.status.write().await.notes_sync_intro_seen = true;
        Ok(())
    }

    pub(super) fn device_id(&self) -> Result<String, EngineError> {
        self.store
            .setting("device_id")?
            .ok_or_else(|| CloudError::Revoked.into())
    }

    pub(super) fn local_client(&self) -> Result<GaggiMateClient, EngineError> {
        let host = self
            .store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".into());
        Ok(GaggiMateClient::new(&host)?)
    }

    pub(super) async fn update_from_cloud_state(&self, value: &Value) {
        let _ = self.apply_sync_interval_from_state(value);
        let items = value
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut profiles = 0;
        let mut shots = 0;
        let mut notes = 0;
        let mut conflicts = 0;
        let mut suppressed = 0;
        for item in items {
            let is_suppressed = item
                .get("suppressed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let is_present = item.get("present").and_then(Value::as_bool).unwrap_or(true);
            if !is_suppressed && is_present {
                match item.get("kind").and_then(Value::as_str) {
                    Some("profile") => profiles += 1,
                    Some("shot") => shots += 1,
                    Some("notes") => notes += 1,
                    _ => {}
                }
            }
            if item
                .get("conflict")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                conflicts += 1;
            }
            if is_suppressed {
                suppressed += 1;
            }
        }
        let source = value.get("source").unwrap_or(&Value::Null);
        let mut status = self.status.write().await;
        status.profiles = profiles;
        status.shots = shots;
        status.notes = notes;
        status.conflicts = conflicts;
        status.suppressed = suppressed;
        status.initial_sync_configured = source
            .get("initial_sync_configured_at")
            .or_else(|| source.get("initialSyncConfiguredAt"))
            .is_some_and(|value| !value.is_null());
        status.duplicate_policy = source
            .get("duplicate_policy")
            .or_else(|| source.get("duplicatePolicy"))
            .and_then(Value::as_str)
            .unwrap_or("reuse_matching")
            .to_string();
        status.notes_sync_status = source
            .get("notes_sync_status")
            .or_else(|| source.get("notesSyncStatus"))
            .and_then(Value::as_str)
            .unwrap_or("one_way")
            .to_string();
        status.notes_sync_target_device_id = source
            .get("notes_sync_target_device_id")
            .or_else(|| source.get("notesSyncTargetDeviceId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        status.notes_sync_writer_device_id = source
            .get("notes_sync_writer_device_id")
            .or_else(|| source.get("notesSyncWriterDeviceId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        status.this_device_id = self.store.setting("device_id").ok().flatten();
        status.notes_sync_intro_seen = self
            .store
            .setting("notes_sync_intro_seen")
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        status.note_backups = value
            .get("noteBackups")
            .or_else(|| value.get("note_backups"))
            .and_then(Value::as_array)
            .map(|backups| {
                backups
                    .iter()
                    .filter_map(|backup| {
                        Some(NoteBackupSummary {
                            id: backup.get("id")?.as_str()?.to_string(),
                            slot: backup.get("slot")?.as_str()?.to_string(),
                            item_count: backup
                                .get("item_count")
                                .or_else(|| backup.get("itemCount"))
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as usize,
                            created_at: backup
                                .get("created_at")
                                .or_else(|| backup.get("createdAt"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            finalized_at: backup
                                .get("finalized_at")
                                .or_else(|| backup.get("finalizedAt"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        status.issues = self.store.failures().unwrap_or_default();
        status.last_sync_at = source
            .get("last_sync_at")
            .or_else(|| source.get("lastSyncAt"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    pub async fn resync_preview(&self) -> Result<Value, EngineError> {
        let device_id = self
            .store
            .setting("device_id")?
            .ok_or(CloudError::Revoked)?;
        let host = self
            .store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".into());
        let local = GaggiMateClient::new(&host)?;
        let mut inventory = Vec::new();
        for (id, _) in local.profiles().await? {
            inventory.push(serde_json::json!({ "kind": "profile", "sourceKey": id }));
        }
        for entry in local.shot_index().await? {
            let source_key = format!("{}:{}", entry.id, entry.timestamp);
            inventory.push(serde_json::json!({ "kind": "shot", "sourceKey": source_key }));
            if local.notes(entry.id).await?.is_some() {
                inventory.push(serde_json::json!({ "kind": "notes", "sourceKey": source_key }));
            }
        }
        self.cloud
            .resync_preview(&device_id, Value::Array(inventory))
            .await
            .map_err(Into::into)
    }

    pub async fn apply_resync(&self, decisions: Value) -> Result<Value, EngineError> {
        let device_id = self
            .store
            .setting("device_id")?
            .ok_or(CloudError::Revoked)?;
        let result = self.cloud.resync_apply(&device_id, decisions).await?;
        self.store.reset_scan_state()?;
        // Do not let the next full scan reuse suppressions cached before the
        // resync was applied. Refresh the authoritative state immediately.
        let state = self.cloud.state(&device_id).await?;
        self.store.set_setting("cloud_state", &state.to_string())?;
        self.update_from_cloud_state(&state).await;
        Ok(result)
    }

    pub async fn disconnect(&self) -> Result<Value, EngineError> {
        let server_revoked = match self.store.setting("device_id")? {
            Some(device_id) => tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.cloud.revoke(&device_id),
            )
            .await
            .is_ok_and(|result| result.is_ok()),
            None => true,
        };
        let credentials_removed = self.credentials.delete_tokens().is_ok();
        self.store.clear_account_data()?;
        self.store.remove_setting("reconnect_required_reason")?;
        self.credentials.delete_pending_device_authorization()?;
        self.store.set_setting("headless_pairing_disabled", "1")?;
        let host = self
            .store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".into());
        *self.status.write().await = AppStatus {
            connected: false,
            machine_host: host,
            machine_reachable: false,
            syncing: false,
            last_sync_at: None,
            last_error: None,
            last_error_code: None,
            last_error_at: None,
            sync_progress: None,
            profiles: 0,
            shots: 0,
            notes: 0,
            conflicts: 0,
            suppressed: 0,
            initial_sync_configured: false,
            duplicate_policy: "reuse_matching".into(),
            notes_sync_status: "one_way".into(),
            notes_sync_target_device_id: None,
            notes_sync_writer_device_id: None,
            this_device_id: None,
            notes_sync_intro_seen: self.store.setting("notes_sync_intro_seen")?.as_deref()
                == Some("1"),
            note_backups: Vec::new(),
            issues: Vec::new(),
        };
        Ok(serde_json::json!({
            "serverRevoked": server_revoked,
            "credentialsRemoved": credentials_removed,
        }))
    }
}
