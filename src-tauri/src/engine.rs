// SPDX-License-Identifier: GPL-3.0-or-later

use std::{collections::HashSet, sync::Arc};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::{
    cloud::{CloudClient, CloudError, PendingOAuth},
    local::{normalize_host, GaggiMateClient, LocalError},
    model::{AppStatus, NoteBackupSummary, SyncObject},
    store::{AppStore, StoreError},
};

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Cloud(#[from] CloudError),
    #[error(transparent)]
    Local(#[from] LocalError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Finish the account connection in the browser first")]
    OAuthState,
    #[error("MyBrewFolio Sync is already running")]
    Busy,
}

impl EngineError {
    fn heartbeat_code(&self) -> &'static str {
        match self {
            Self::Cloud(CloudError::NotConfigured) => "SYNC_OAUTH_NOT_CONFIGURED",
            Self::Cloud(CloudError::OAuth) => "SYNC_OAUTH_FAILED",
            Self::Cloud(CloudError::Revoked) => "SYNC_DEVICE_REVOKED",
            Self::Cloud(CloudError::Unreachable) => "MYBREWFOLIO_UNREACHABLE",
            Self::Cloud(CloudError::Rejected) => "SYNC_DATA_REJECTED",
            Self::Local(LocalError::InvalidHost) => "GAGGIMATE_HOST_INVALID",
            Self::Local(LocalError::Unreachable) => "GAGGIMATE_UNREACHABLE",
            Self::Local(LocalError::InvalidData) => "GAGGIMATE_DATA_INVALID",
            Self::Store(StoreError::Database(_)) => "LOCAL_DATABASE_ERROR",
            Self::Store(StoreError::Keychain) => "SYSTEM_KEYCHAIN_UNAVAILABLE",
            Self::Store(StoreError::InvalidCredentials) => "LOCAL_CREDENTIALS_INVALID",
            Self::OAuthState => "SYNC_OAUTH_STATE_INVALID",
            Self::Busy => "SYNC_ALREADY_RUNNING",
        }
    }
}

pub struct SyncEngine {
    store: Arc<AppStore>,
    cloud: CloudClient,
    pending_oauth: Mutex<Option<PendingOAuth>>,
    status: RwLock<AppStatus>,
    sync_lock: Mutex<()>,
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                integer.to_string()
            } else if let Some(integer) = value.as_u64() {
                integer.to_string()
            } else if let Some(float) = value.as_f64() {
                if float == 0.0 {
                    "0".to_string()
                } else if float.fract() == 0.0 && float.abs() < 1e21 {
                    format!("{float:.0}")
                } else {
                    value.to_string()
                }
            } else {
                value.to_string()
            }
        }
        Value::String(value) => serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into()),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()),
                        canonical_json(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

fn hash_value(value: &Value) -> String {
    hex_digest(canonical_json(value).as_bytes())
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn platform() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        return "windows";
    }
    #[cfg(target_os = "macos")]
    {
        return "macos";
    }
    #[cfg(target_os = "linux")]
    {
        return "linux";
    }
    #[allow(unreachable_code)]
    "linux"
}

fn installation_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        return "Windows computer";
    }
    #[cfg(target_os = "macos")]
    {
        return "macOS computer";
    }
    #[cfg(target_os = "linux")]
    {
        return "Linux computer";
    }
    #[allow(unreachable_code)]
    "Computer"
}

fn parse_time(value: Option<String>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn api_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl SyncEngine {
    pub fn open(store: Arc<AppStore>) -> Result<Self, EngineError> {
        let cloud = CloudClient::new(store.clone())?;
        let host = store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".to_string());
        let connected = store.tokens()?.is_some() && store.setting("device_id")?.is_some();
        let device_id = store.setting("device_id")?;
        let notes_sync_intro_seen = store.setting("notes_sync_intro_seen")?.as_deref() == Some("1");
        let issues = store.failures().unwrap_or_default();
        Ok(Self {
            store,
            cloud,
            pending_oauth: Mutex::new(None),
            status: RwLock::new(AppStatus {
                connected,
                machine_host: host,
                machine_reachable: false,
                syncing: false,
                last_sync_at: None,
                last_error: None,
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
            sync_lock: Mutex::new(()),
        })
    }

    pub async fn status(&self) -> AppStatus {
        self.status.read().await.clone()
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

    fn device_id(&self) -> Result<String, EngineError> {
        self.store
            .setting("device_id")?
            .ok_or_else(|| CloudError::Revoked.into())
    }

    fn local_client(&self) -> Result<GaggiMateClient, EngineError> {
        let host = self
            .store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".into());
        Ok(GaggiMateClient::new(&host)?)
    }

    async fn create_notes_backup(&self, slot: &str) -> Result<String, EngineError> {
        let device_id = self.device_id()?;
        let local = self.local_client()?;
        let started = self.cloud.begin_notes_backup(&device_id, slot).await?;
        let backup_id = started
            .get("backup")
            .and_then(|backup| backup.get("id"))
            .and_then(Value::as_str)
            .ok_or(CloudError::Rejected)?
            .to_string();
        let mut items = Vec::new();
        for entry in local.shot_index().await? {
            let notes = local
                .notes(entry.id)
                .await?
                .unwrap_or_else(|| serde_json::json!({}));
            items.push(serde_json::json!({
                "sourceKey": format!("{}:{}", entry.id, entry.timestamp),
                "machineShotId": entry.id.to_string(),
                "shotTimestamp": entry.timestamp,
                "notesHash": hash_value(&notes),
                "notes": notes,
            }));
        }
        for chunk in items.chunks(25) {
            self.cloud
                .add_notes_backup_items(&device_id, &backup_id, chunk)
                .await?;
        }
        let inventory_hash = hash_value(&Value::Array(items));
        self.cloud
            .finalize_notes_backup(&device_id, &backup_id, &inventory_hash)
            .await?;
        Ok(backup_id)
    }

    pub async fn begin_two_way_notes_activation(&self) -> Result<Value, EngineError> {
        let device_id = self.device_id()?;
        let status = self.status.read().await.clone();
        if status.notes_sync_status == "one_way" {
            self.cloud.request_two_way_notes(&device_id).await?;
        } else if status.notes_sync_status == "two_way" {
            return Err(CloudError::Rejected.into());
        } else if status.notes_sync_target_device_id.as_deref() != Some(device_id.as_str()) {
            return Err(CloudError::Rejected.into());
        }
        self.dismiss_notes_sync_intro().await?;
        let backup_id = self.create_notes_backup("activation").await?;
        let mut preview = self
            .cloud
            .notes_activation_preview(&device_id, &backup_id)
            .await?;
        if let Some(object) = preview.as_object_mut() {
            object.insert("backupId".into(), Value::String(backup_id));
        }
        Ok(preview)
    }

    pub async fn activate_two_way_notes(
        &self,
        backup_id: &str,
        decisions: Value,
    ) -> Result<Value, EngineError> {
        let device_id = self.device_id()?;
        let result = self
            .cloud
            .activate_two_way_notes(&device_id, backup_id, decisions)
            .await?;
        let state = self.cloud.state(&device_id).await?;
        self.store.set_setting("cloud_state", &state.to_string())?;
        self.update_from_cloud_state(&state).await;
        Ok(result)
    }

    pub async fn disable_two_way_notes(&self) -> Result<(), EngineError> {
        let device_id = self.device_id()?;
        self.cloud.disable_two_way_notes(&device_id).await?;
        let state = self.cloud.state(&device_id).await?;
        self.store.set_setting("cloud_state", &state.to_string())?;
        self.update_from_cloud_state(&state).await;
        Ok(())
    }

    pub async fn create_latest_notes_backup(&self) -> Result<String, EngineError> {
        let backup_id = self.create_notes_backup("latest").await?;
        let device_id = self.device_id()?;
        let state = self.cloud.state(&device_id).await?;
        self.store.set_setting("cloud_state", &state.to_string())?;
        self.update_from_cloud_state(&state).await;
        Ok(backup_id)
    }

    pub async fn preview_notes_restore(&self, backup_id: &str) -> Result<Value, EngineError> {
        let device_id = self.device_id()?;
        let local = self.local_client()?;
        let index: HashSet<String> = local
            .shot_index()
            .await?
            .into_iter()
            .map(|entry| format!("{}:{}", entry.id, entry.timestamp))
            .collect();
        let mut backup = self.cloud.notes_backup_items(&device_id, backup_id).await?;
        if let Some(items) = backup.get_mut("items").and_then(Value::as_array_mut) {
            for item in items {
                let source_key = item
                    .get("source_key")
                    .or_else(|| item.get("sourceKey"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(object) = item.as_object_mut() {
                    object.insert("available".into(), Value::Bool(index.contains(&source_key)));
                }
            }
        }
        Ok(backup)
    }

    pub async fn restore_notes_backup(
        &self,
        backup_id: &str,
        source_keys: &[String],
    ) -> Result<Value, EngineError> {
        let device_id = self.device_id()?;
        self.create_notes_backup("latest").await?;
        let local = self.local_client()?;
        let index: HashSet<String> = local
            .shot_index()
            .await?
            .into_iter()
            .map(|entry| format!("{}:{}", entry.id, entry.timestamp))
            .collect();
        let backup = self.cloud.notes_backup_items(&device_id, backup_id).await?;
        let requested: HashSet<&str> = source_keys.iter().map(String::as_str).collect();
        let mut verified = Vec::new();
        let mut skipped = 0;
        for item in backup
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(source_key) = item
                .get("source_key")
                .or_else(|| item.get("sourceKey"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            if !requested.is_empty() && !requested.contains(source_key) {
                continue;
            }
            if !index.contains(source_key) {
                skipped += 1;
                continue;
            }
            let Some(id) = source_key
                .split(':')
                .next()
                .and_then(|value| value.parse::<u32>().ok())
            else {
                skipped += 1;
                continue;
            };
            let notes = item
                .get("notes_data")
                .or_else(|| item.get("notes"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let actual = local.save_notes(id, &notes).await?;
            let actual_hash = hash_value(&actual);
            let expected_hash = item
                .get("notes_hash")
                .or_else(|| item.get("notesHash"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if actual_hash == expected_hash {
                verified.push(
                    serde_json::json!({ "sourceKey": source_key, "verifiedHash": actual_hash }),
                );
            } else {
                skipped += 1;
            }
        }
        let applied = self
            .cloud
            .apply_notes_restore_results(&device_id, backup_id, Value::Array(verified))
            .await?;
        Ok(
            serde_json::json!({ "applied": applied.get("applied").and_then(Value::as_u64).unwrap_or(0), "skipped": skipped }),
        )
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
        let mut status = self.status.write().await;
        status.connected = true;
        status.this_device_id = Some(device.id);
        status.last_error = None;
        Ok(())
    }

    async fn update_from_cloud_state(&self, value: &Value) {
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

    async fn queue_local_changes(
        &self,
        local: &GaggiMateClient,
        cloud_state: &Value,
    ) -> Result<Vec<String>, EngineError> {
        let mut skipped = Vec::new();
        let items = cloud_state
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let suppressed: HashSet<(String, String)> = items
            .iter()
            .filter_map(|item| {
                if !item
                    .get("suppressed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return None;
                }
                Some((
                    item.get("kind")?.as_str()?.to_string(),
                    item.get("source_key")
                        .or_else(|| item.get("sourceKey"))?
                        .as_str()?
                        .to_string(),
                ))
            })
            .collect();

        let now = Utc::now();
        let last_profiles = parse_time(self.store.setting("last_profile_scan")?);
        if last_profiles.map_or(true, |last| now - last >= Duration::minutes(5)) {
            for (id, loaded) in local.profiles().await? {
                if suppressed.contains(&("profile".into(), id.clone())) {
                    continue;
                }
                let data = match loaded {
                    Ok(data) => data,
                    Err(_) => {
                        self.store.record_failure(
                            None,
                            "profile",
                            &id,
                            "read",
                            "Profile could not be read from the GaggiMate",
                        )?;
                        skipped.push(format!("Profile {id} could not be read"));
                        continue;
                    }
                };
                self.store.clear_failure("profile", &id)?;
                self.store.queue(&SyncObject {
                    kind: "profile".into(),
                    source_key: id,
                    source_hash: hash_value(&data),
                    shot_source_key: None,
                    data,
                })?;
            }
            self.store
                .set_setting("last_profile_scan", &now.to_rfc3339())?;
        }

        if self.store.setting("notes_reader_version")?.as_deref() != Some("2") {
            self.store.set_setting("last_full_notes_scan", "")?;
            self.store.set_setting("last_recent_notes_scan", "")?;
            self.store.set_setting("notes_reader_version", "2")?;
        }
        let full_notes = parse_time(self.store.setting("last_full_notes_scan")?)
            .map_or(true, |last| now - last >= Duration::days(1));
        let recent_notes = parse_time(self.store.setting("last_recent_notes_scan")?)
            .map_or(true, |last| now - last >= Duration::minutes(5));
        let index = local.shot_index().await?;
        for (position, entry) in index.into_iter().enumerate() {
            // IDs can be reused after history maintenance. The timestamp keeps a
            // later shot from silently replacing an older cloud copy.
            let source_key = format!("{}:{}", entry.id, entry.timestamp);
            if suppressed.contains(&("shot".into(), source_key.clone())) {
                continue;
            }
            let fingerprint = format!(
                "{}:{}:{}:{}",
                entry.timestamp,
                entry.duration,
                entry.volume.unwrap_or_default(),
                entry.rating.unwrap_or_default()
            );
            // v2 deliberately requeues shots once after the original client
            // used non-canonical JSON hashes that the API could not accept.
            let fingerprint_key = format!("shot_fingerprint_v2:{source_key}");
            let changed = self.store.setting(&fingerprint_key)?.as_deref() != Some(&fingerprint);
            if changed {
                let mut shot = match local.shot(entry.id).await {
                    Ok(shot) => shot,
                    Err(_) => {
                        self.store.record_failure(
                            None,
                            "shot",
                            &source_key,
                            "read",
                            "Shot could not be read from the GaggiMate",
                        )?;
                        skipped.push(format!("Shot {} could not be read", entry.id));
                        continue;
                    }
                };
                self.store.clear_failure("shot", &source_key)?;
                if let Some(object) = shot.as_object_mut() {
                    object.insert(
                        "name".into(),
                        Value::String(format!("{} · {}", entry.profile_name, entry.id)),
                    );
                    object.insert("rating".into(), serde_json::json!(entry.rating));
                    object.insert("volume".into(), serde_json::json!(entry.volume));
                }
                self.store.queue(&SyncObject {
                    kind: "shot".into(),
                    source_key: source_key.clone(),
                    source_hash: hash_value(&shot),
                    shot_source_key: None,
                    data: shot,
                })?;
                self.store.set_setting(&fingerprint_key, &fingerprint)?;
            }
            let refresh_recent_notes = recent_notes && position < 20;
            if (changed || full_notes || refresh_recent_notes)
                && !suppressed.contains(&("notes".into(), source_key.clone()))
            {
                match local.notes(entry.id).await {
                    Ok(Some(notes)) => {
                        self.store.clear_failure("notes", &source_key)?;
                        self.store.queue(&SyncObject {
                            kind: "notes".into(),
                            source_key: source_key.clone(),
                            source_hash: hash_value(&notes),
                            shot_source_key: Some(source_key.clone()),
                            data: notes,
                        })?;
                    }
                    Ok(None) => {
                        self.store.clear_failure("notes", &source_key)?;
                    }
                    Err(_) => {
                        let reason = "Notes could not be read from the GaggiMate";
                        self.store
                            .record_failure(None, "notes", &source_key, "read", reason)?;
                        skipped.push(format!("Notes for shot {} could not be read", entry.id))
                    }
                }
            }
        }
        if recent_notes {
            self.store
                .set_setting("last_recent_notes_scan", &now.to_rfc3339())?;
        }
        if full_notes {
            self.store
                .set_setting("last_full_notes_scan", &now.to_rfc3339())?;
        }
        Ok(skipped)
    }

    async fn flush_queue(&self, device_id: &str) -> Result<usize, EngineError> {
        let mut invalid = 0;
        loop {
            let pending = self.store.pending(25)?;
            if pending.is_empty() {
                break;
            }
            let response = self.cloud.batch(device_id, &pending).await?;
            let results = response
                .get("results")
                .and_then(Value::as_array)
                .ok_or(CloudError::Rejected)?;
            if results.is_empty() {
                break;
            }
            for result in results {
                let index = result
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(u64::MAX) as usize;
                let status = result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid");
                if status == "invalid" {
                    invalid += 1;
                }
                if let Some(object) = pending.get(index) {
                    if status == "invalid" {
                        let reason = result
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("MyBrewFolio rejected this item");
                        self.store.record_failure(
                            Some(object),
                            &object.kind,
                            &object.source_key,
                            "upload",
                            reason,
                        )?;
                    } else {
                        self.store.clear_failure(&object.kind, &object.source_key)?;
                    }
                    if matches!(
                        status,
                        "created"
                            | "updated"
                            | "linked"
                            | "unchanged"
                            | "suppressed"
                            | "conflict"
                            | "invalid"
                    ) {
                        self.store
                            .remove_pending(&object.kind, &object.source_key)?;
                    }
                }
            }
            if pending.len() < 25 {
                break;
            }
        }
        Ok(invalid)
    }

    async fn process_outbound_notes(
        &self,
        local: &GaggiMateClient,
        device_id: &str,
    ) -> Result<(), EngineError> {
        let mut claim = self.cloud.claim_outbound_notes(device_id).await?;
        if claim.get("status").and_then(Value::as_str) == Some("backup_required") {
            self.create_notes_backup("latest").await?;
            claim = self.cloud.claim_outbound_notes(device_id).await?;
        }
        for operation in claim
            .get("operations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let operation_id = operation
                .get("id")
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?;
            let lease_token = operation
                .get("leaseToken")
                .or_else(|| operation.get("lease_token"))
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?;
            let source_key = operation
                .get("sourceKey")
                .or_else(|| operation.get("source_key"))
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?;
            let machine_id = source_key
                .split(':')
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or(CloudError::Rejected)?;
            let current = local
                .notes(machine_id)
                .await?
                .unwrap_or_else(|| serde_json::json!({}));
            let current_hash = hash_value(&current);
            let base_hash = operation
                .get("baseSourceHash")
                .or_else(|| operation.get("base_source_hash"))
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?;
            if current_hash != base_hash {
                self.cloud
                    .complete_outbound_note(
                        device_id,
                        operation_id,
                        serde_json::json!({
                            "leaseToken": lease_token,
                            "status": "conflict",
                            "actualHash": current_hash,
                            "actualNotes": current,
                        }),
                    )
                    .await?;
                continue;
            }
            let desired = operation
                .get("desiredNotes")
                .or_else(|| operation.get("desired_data"))
                .cloned()
                .ok_or(CloudError::Rejected)?;
            match local.save_notes(machine_id, &desired).await {
                Ok(actual) => {
                    self.cloud
                        .complete_outbound_note(
                            device_id,
                            operation_id,
                            serde_json::json!({
                                "leaseToken": lease_token,
                                "status": "applied",
                                "actualHash": hash_value(&actual),
                            }),
                        )
                        .await?;
                }
                Err(error) => {
                    self.cloud
                        .complete_outbound_note(
                            device_id,
                            operation_id,
                            serde_json::json!({
                                "leaseToken": lease_token,
                                "status": "failed",
                                "error": error.to_string(),
                            }),
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    pub async fn sync_once(&self) -> Result<(), EngineError> {
        let guard = self.sync_lock.try_lock().map_err(|_| EngineError::Busy)?;
        let device_id = self
            .store
            .setting("device_id")?
            .ok_or(CloudError::Revoked)?;
        {
            let mut status = self.status.write().await;
            status.syncing = true;
            status.last_error = None;
        }
        let host = self
            .store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".into());
        let result = async {
            let (state, cloud_unreachable) = match self.cloud.state(&device_id).await {
                Ok(state) => {
                    self.store.set_setting("cloud_state", &state.to_string())?;
                    self.update_from_cloud_state(&state).await;
                    (state, false)
                }
                Err(CloudError::Unreachable) => {
                    let cached = self
                        .store
                        .setting("cloud_state")?
                        .and_then(|value| serde_json::from_str(&value).ok())
                        .unwrap_or_else(|| serde_json::json!({ "items": [] }));
                    (cached, true)
                }
                Err(error) => return Err(error.into()),
            };
            let configured = state
                .get("source")
                .and_then(|source| {
                    source
                        .get("initial_sync_configured_at")
                        .or_else(|| source.get("initialSyncConfiguredAt"))
                })
                .is_some_and(|value| !value.is_null());
            if !configured {
                return Ok(());
            }
            let local = GaggiMateClient::new(&host)?;
            let skipped = self.queue_local_changes(&local, &state).await?;
            if cloud_unreachable {
                // Local changes are safely queued before reporting the missing
                // internet connection. They are uploaded on the next cycle.
                return Err(CloudError::Unreachable.into());
            }
            let invalid = self.flush_queue(&device_id).await?;
            self.process_outbound_notes(&local, &device_id).await?;
            let synchronized_at = api_timestamp(Utc::now());
            let warning_code =
                (!skipped.is_empty() || invalid > 0).then_some("LOCAL_ITEMS_SKIPPED");
            self.cloud
                .heartbeat(&device_id, true, Some(&synchronized_at), warning_code)
                .await?;
            let refreshed = self.cloud.state(&device_id).await?;
            self.store
                .set_setting("cloud_state", &refreshed.to_string())?;
            self.update_from_cloud_state(&refreshed).await;
            let mut status = self.status.write().await;
            status.machine_reachable = true;
            status.last_sync_at = Some(synchronized_at);
            status.last_error = warning_code.map(|_| {
                format!(
                    "{} local files could not be synchronized. Review the details below.",
                    skipped.len() + invalid
                )
            });
            status.issues = self.store.failures().unwrap_or_default();
            Ok::<(), EngineError>(())
        }
        .await;
        if let Err(error) = &result {
            if matches!(error, EngineError::Cloud(CloudError::Revoked)) {
                // Device revocation is checked by the API for every request.
                // Clear credentials and queued account data immediately so a
                // revoked installation cannot keep presenting itself as linked.
                self.store.delete_tokens()?;
                self.store.clear_account_data()?;
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
                    last_error: Some(error.to_string()),
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
                drop(guard);
                return result;
            }
            let message = error.to_string();
            let (machine_reachable, last_sync_at) = {
                let mut status = self.status.write().await;
                status.machine_reachable = !matches!(error, EngineError::Local(_));
                status.last_error = Some(message.clone());
                (status.machine_reachable, status.last_sync_at.clone())
            };
            let _ = self
                .cloud
                .heartbeat(
                    &device_id,
                    machine_reachable,
                    last_sync_at.as_deref(),
                    Some(error.heartbeat_code()),
                )
                .await;
        }
        self.status.write().await.syncing = false;
        drop(guard);
        result
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
        let credentials_removed = self.store.delete_tokens().is_ok();
        self.store.clear_account_data()?;
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

    pub async fn emit_status(&self, app: &tauri::AppHandle) {
        let status = self.status().await;
        if let Some(item) = app.try_state::<crate::TrayStatusItem>() {
            let text = if status.syncing {
                "Syncing…"
            } else if status.last_error.is_some() {
                "Items not synchronized"
            } else if status.connected {
                "MyBrewFolio connected"
            } else {
                "MyBrewFolio not connected"
            };
            let _ = item.0.set_text(text);
        }
        if let Some(item) = app.try_state::<crate::TrayMachineItem>() {
            let _ = item.0.set_text(format!("Machine: {}", status.machine_host));
        }
        if let Some(item) = app.try_state::<crate::TrayErrorItem>() {
            let text = status
                .last_error
                .as_deref()
                .map(|error| {
                    let shortened: String = error.chars().take(90).collect();
                    format!("Last error: {shortened}")
                })
                .unwrap_or_else(|| "No Sync errors".to_string());
            let _ = item.0.set_text(text);
        }
        let _ = app.emit("sync-status-changed", status);
    }
}

#[cfg(test)]
mod tests {
    use super::{api_timestamp, hash_value};
    use chrono::{TimeZone, Utc};

    #[test]
    fn hashes_json_like_the_sync_api() {
        let value = serde_json::json!({
            "b": 1,
            "a": [true, { "z": null, "x": "café" }],
            "n": 1.25
        });
        assert_eq!(
            hash_value(&value),
            "383410d6c75c6bd29378b3b9da39e37fde1ab284f8f1cb0230ecc2c196f5d346"
        );
    }

    #[test]
    fn normalizes_integer_shaped_numbers() {
        let value = serde_json::json!({
            "samples": [0, 1.0, 1.25, -2.5],
            "name": "Shot"
        });
        assert_eq!(
            hash_value(&value),
            "8022fd9de6812be583b599abbf16921a856d33314b7c0db32dba8b9d515b0f3f"
        );
    }

    #[test]
    fn formats_api_timestamps_as_utc_zulu_time() {
        let timestamp = Utc
            .with_ymd_and_hms(2026, 7, 27, 9, 41, 33)
            .single()
            .expect("valid timestamp");
        assert_eq!(api_timestamp(timestamp), "2026-07-27T09:41:33.000Z");
    }
}
