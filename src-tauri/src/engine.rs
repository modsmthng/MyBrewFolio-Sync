// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration as StdDuration,
};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::{
    cloud::{
        CloudClient, CloudError, DeviceAuthorizationInfo, PendingDeviceAuthorization, PendingOAuth,
    },
    credentials::CredentialStore,
    local::{normalize_host, GaggiMateClient, LocalError},
    model::{AppStatus, IndexEntry, NoteBackupSummary, SyncObject},
    store::{AppStore, StoreError},
};

const MAX_SYNC_BATCH_ITEMS: usize = 25;
// The API accepts 8 MiB batches. Keep a margin so metadata added by a future
// client version cannot turn an otherwise valid queue entry into a rejected
// request.
const MAX_SYNC_BATCH_BYTES: usize = 7 * 1024 * 1024;
const NOTES_WRITE_ATTEMPTS: usize = 3;
const NOTES_WRITE_RETRY_DELAYS: [StdDuration; NOTES_WRITE_ATTEMPTS - 1] = [
    StdDuration::from_millis(250),
    // The third attempt is scheduled 750 ms after the first, not 750 ms
    // after the second.
    StdDuration::from_millis(500),
];

fn serialized_batch_bytes(items: &[SyncObject]) -> usize {
    serde_json::to_vec(&serde_json::json!({ "items": items }))
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

fn select_sync_batch(pending: &[SyncObject]) -> (Vec<SyncObject>, Option<SyncObject>) {
    let mut selected = Vec::new();
    for object in pending {
        let mut candidate = selected.clone();
        candidate.push(object.clone());
        if serialized_batch_bytes(&candidate) <= MAX_SYNC_BATCH_BYTES {
            selected.push(object.clone());
        } else if selected.is_empty() {
            return (selected, Some(object.clone()));
        } else {
            break;
        }
    }
    (selected, None)
}

fn suppressed_items(cloud_state: &Value) -> HashSet<(String, String)> {
    cloud_state
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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
        .collect()
}

fn two_way_notes_active(cloud_state: &Value) -> bool {
    cloud_state
        .get("source")
        .and_then(|source| {
            source
                .get("notes_sync_status")
                .or_else(|| source.get("notesSyncStatus"))
        })
        .and_then(Value::as_str)
        == Some("two_way")
}

fn note_source_hashes(cloud_state: &Value) -> HashMap<String, String> {
    cloud_state
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if item.get("kind").and_then(Value::as_str) != Some("notes")
                || item
                    .get("suppressed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            {
                return None;
            }
            Some((
                item.get("source_key")
                    .or_else(|| item.get("sourceKey"))?
                    .as_str()?
                    .to_string(),
                item.get("source_hash")
                    .or_else(|| item.get("sourceHash"))?
                    .as_str()?
                    .to_string(),
            ))
        })
        .collect()
}

fn zero_notes_value(value: &Value) -> bool {
    value.as_f64().is_some_and(|number| number == 0.0)
        || value
            .as_str()
            .is_some_and(|text| matches!(text.trim(), "" | "0" | "0.0"))
}

/// GaggiMate can persist an untouched Notes form as a fully populated default
/// object. It has the same meaning as an absent Notes record and must never
/// silently clear a cloud Note.
fn notes_are_semantically_empty(notes: &Value) -> bool {
    let Some(values) = notes.as_object() else {
        return false;
    };
    values.iter().all(|(key, value)| match key.as_str() {
        "id" | "timestamp" => true,
        "rating" | "doseIn" | "doseOut" | "ratio" => value.is_null() || zero_notes_value(value),
        "balanceTaste" => {
            value.is_null()
                || value.as_str().is_some_and(|text| {
                    text.trim().is_empty() || text.trim().eq_ignore_ascii_case("balanced")
                })
        }
        "beanType" | "grindSetting" | "notes" => {
            value.is_null() || value.as_str().is_some_and(|text| text.trim().is_empty())
        }
        // Unknown fields are considered user content unless they are null.
        _ => value.is_null(),
    })
}

fn normalized_notes(notes: Value) -> Value {
    if notes_are_semantically_empty(&notes) {
        json!({})
    } else {
        notes
    }
}

enum NotesWriteOutcome {
    Applied(Value),
    Conflict(Value),
    Unverified,
}

fn scan_due(now: DateTime<Utc>, last: Option<DateTime<Utc>>, interval: Duration) -> bool {
    last.map_or(true, |last| now - last >= interval)
}

fn shot_source_key(entry: &IndexEntry) -> String {
    format!("{}:{}", entry.id, entry.timestamp)
}

fn shot_fingerprint(entry: &IndexEntry) -> String {
    format!(
        "{}:{}:{}:{}",
        entry.timestamp,
        entry.duration,
        entry.volume.unwrap_or_default(),
        entry.rating.unwrap_or_default()
    )
}

fn should_refresh_notes(
    changed: bool,
    full_scan: bool,
    recent_scan: bool,
    position: usize,
) -> bool {
    changed || full_scan || (recent_scan && position < 20)
}

fn shot_read_failure(error: &LocalError) -> String {
    match error {
        LocalError::UnsupportedShotFormat(version) => {
            format!("GaggiMate shot format v{version} is not supported by this MyBrewFolio Sync version")
        }
        _ => "Shot could not be read from the GaggiMate".into(),
    }
}

fn batch_result_status(result: &Value) -> (usize, &str) {
    (
        result
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX) as usize,
        result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("invalid"),
    )
}

fn is_terminal_batch_status(status: &str) -> bool {
    matches!(
        status,
        "created" | "updated" | "linked" | "unchanged" | "suppressed" | "conflict" | "invalid"
    )
}

fn normalized_profile(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(normalized_profile).collect()),
        Value::Object(object) => {
            let sorted = object
                .iter()
                .filter(|(key, _)| *key != "favorite" && *key != "selected")
                .map(|(key, value)| (key.clone(), normalized_profile(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::to_value(sorted).unwrap_or(Value::Null)
        }
        _ => value.clone(),
    }
}

fn profiles_equal(left: &Value, right: &Value) -> bool {
    normalized_profile(left) == normalized_profile(right)
}

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
            Self::Local(LocalError::UnsupportedShotFormat(_)) => {
                "GAGGIMATE_SHOT_FORMAT_UNSUPPORTED"
            }
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
    credentials: Arc<dyn CredentialStore>,
    pending_oauth: Mutex<Option<PendingOAuth>>,
    status: RwLock<AppStatus>,
    sync_lock: Mutex<()>,
    profile_store_lock: Mutex<()>,
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
    pub fn open(
        store: Arc<AppStore>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, EngineError> {
        let cloud = CloudClient::new(credentials.clone())?;
        let host = store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".to_string());
        let connected = credentials.tokens()?.is_some() && store.setting("device_id")?.is_some();
        let device_id = store.setting("device_id")?;
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
            profile_store_lock: Mutex::new(()),
        })
    }

    pub async fn status(&self) -> AppStatus {
        self.status.read().await.clone()
    }

    /// Returns local, read-only diagnostics. This intentionally reports the
    /// cached cloud state so asking for help never triggers another import or
    /// changes duplicate handling.
    pub async fn diagnose(&self) -> Result<Value, EngineError> {
        let status = self.status().await;
        let pending = self.store.pending_count()?;
        let failures = self.store.failure_count()?;
        Ok(json!({
            "connection": {
                "connected": status.connected,
                "machineHost": status.machine_host,
                "machineReachable": status.machine_reachable,
                "syncing": status.syncing,
                "lastSyncAt": status.last_sync_at,
                "lastError": status.last_error,
            },
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
            let notes = normalized_notes(
                local
                    .notes(entry.id)
                    .await?
                    .unwrap_or_else(|| serde_json::json!({})),
            );
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
        self.register_connected_device().await
    }

    pub async fn begin_device_oauth(&self) -> Result<DeviceAuthorizationInfo, EngineError> {
        let (info, pending) = self.cloud.begin_device_authorization().await?;
        let value = serde_json::to_string(&pending).map_err(|_| StoreError::InvalidCredentials)?;
        self.credentials.save_pending_device_authorization(&value)?;
        Ok(info)
    }

    pub async fn poll_device_oauth(&self) -> Result<bool, EngineError> {
        let value = self
            .credentials
            .pending_device_authorization()?
            .ok_or(EngineError::OAuthState)?;
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
        let suppressed = suppressed_items(cloud_state);
        let two_way_notes = two_way_notes_active(cloud_state);
        let existing_note_hashes = note_source_hashes(cloud_state);
        let empty_notes_hash = hash_value(&json!({}));

        let now = Utc::now();
        let last_profiles = parse_time(self.store.setting("last_profile_scan")?);
        if scan_due(now, last_profiles, Duration::minutes(5)) {
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
                self.store.clear_failure_stage("profile", &id, "read")?;
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

        if self.store.setting("notes_reader_version")?.as_deref() != Some("3") {
            self.store.set_setting("last_full_notes_scan", "")?;
            self.store.set_setting("last_recent_notes_scan", "")?;
            self.store.set_setting("notes_reader_version", "3")?;
        }
        let full_notes = scan_due(
            now,
            parse_time(self.store.setting("last_full_notes_scan")?),
            Duration::days(1),
        );
        let recent_notes = scan_due(
            now,
            parse_time(self.store.setting("last_recent_notes_scan")?),
            Duration::minutes(5),
        );
        let index = local.shot_index().await?;
        for (position, entry) in index.into_iter().enumerate() {
            // IDs can be reused after history maintenance. The timestamp keeps a
            // later shot from silently replacing an older cloud copy.
            let source_key = shot_source_key(&entry);
            if suppressed.contains(&("shot".into(), source_key.clone())) {
                continue;
            }
            let fingerprint = shot_fingerprint(&entry);
            // v2 deliberately requeues shots once after the original client
            // used non-canonical JSON hashes that the API could not accept.
            let fingerprint_key = format!("shot_fingerprint_v2:{source_key}");
            let changed = self.store.setting(&fingerprint_key)?.as_deref() != Some(&fingerprint);
            if changed {
                let mut shot = match local.shot(entry.id).await {
                    Ok(shot) => shot,
                    Err(error) => {
                        let reason = shot_read_failure(&error);
                        self.store
                            .record_failure(None, "shot", &source_key, "read", &reason)?;
                        skipped.push(format!("Shot {} could not be read: {reason}", entry.id));
                        continue;
                    }
                };
                self.store
                    .clear_failure_stage("shot", &source_key, "read")?;
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
            if should_refresh_notes(changed, full_notes, recent_notes, position)
                && !suppressed.contains(&("notes".into(), source_key.clone()))
            {
                match local.notes(entry.id).await {
                    Ok(notes) => {
                        self.store
                            .clear_failure_stage("notes", &source_key, "read")?;
                        let notes =
                            normalized_notes(notes.unwrap_or_else(|| serde_json::json!({})));
                        let empty = notes_are_semantically_empty(&notes);
                        let was_verified_nonempty = existing_note_hashes
                            .get(&source_key)
                            .is_some_and(|hash| hash != &empty_notes_hash);
                        // A missing Notes record is normally ignored. In active
                        // two-way mode it becomes a clear candidate only after a
                        // non-empty machine version was verified previously.
                        if !empty || (two_way_notes && was_verified_nonempty) {
                            self.store.queue(&SyncObject {
                                kind: "notes".into(),
                                source_key: source_key.clone(),
                                source_hash: hash_value(&notes),
                                shot_source_key: Some(source_key.clone()),
                                data: notes,
                            })?;
                        }
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
            let pending = self.store.pending(MAX_SYNC_BATCH_ITEMS)?;
            if pending.is_empty() {
                break;
            }
            let (batch, oversized) = select_sync_batch(&pending);
            if let Some(object) = oversized {
                invalid += 1;
                self.store.record_failure(
                    Some(&object),
                    &object.kind,
                    &object.source_key,
                    "upload",
                    "This item exceeds the maximum MyBrewFolio upload batch size",
                )?;
                self.store
                    .remove_pending(&object.kind, &object.source_key)?;
                continue;
            }
            let response = self.cloud.batch(device_id, &batch).await?;
            let results = response
                .get("results")
                .and_then(Value::as_array)
                .ok_or(CloudError::Rejected)?;
            if results.is_empty() {
                break;
            }
            for result in results {
                let (index, status) = batch_result_status(result);
                if status == "invalid" {
                    invalid += 1;
                }
                if let Some(object) = batch.get(index) {
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
                        self.store.clear_failure_stage(
                            &object.kind,
                            &object.source_key,
                            "upload",
                        )?;
                    }
                    if is_terminal_batch_status(status) {
                        self.store
                            .remove_pending(&object.kind, &object.source_key)?;
                    }
                }
            }
        }
        Ok(invalid)
    }

    async fn write_and_verify_notes(
        &self,
        local: &GaggiMateClient,
        machine_id: u32,
        base_hash: &str,
        desired: &Value,
    ) -> Result<NotesWriteOutcome, LocalError> {
        let desired = normalized_notes(desired.clone());
        let desired_hash = hash_value(&desired);
        for attempt in 0..NOTES_WRITE_ATTEMPTS {
            local.write_notes(machine_id, &desired).await?;
            let actual = normalized_notes(
                local
                    .notes(machine_id)
                    .await?
                    .unwrap_or_else(|| serde_json::json!({})),
            );
            let actual_hash = hash_value(&actual);
            if actual_hash == desired_hash {
                return Ok(NotesWriteOutcome::Applied(actual));
            }
            // A value other than the version we compared before writing is a
            // concurrent machine edit, not a reason to overwrite it again.
            if actual_hash != base_hash {
                return Ok(NotesWriteOutcome::Conflict(actual));
            }
            if let Some(delay) = NOTES_WRITE_RETRY_DELAYS.get(attempt) {
                tokio::time::sleep(*delay).await;
            }
        }
        Ok(NotesWriteOutcome::Unverified)
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
            let base_hash = operation
                .get("baseSourceHash")
                .or_else(|| operation.get("base_source_hash"))
                .and_then(Value::as_str)
                .ok_or(CloudError::Rejected)?;
            let current = match local.notes(machine_id).await {
                Ok(notes) => normalized_notes(notes.unwrap_or_else(|| serde_json::json!({}))),
                Err(error) => {
                    let reason = "GaggiMate could not be reached to verify the Notes update. Sync will retry automatically.";
                    self.store
                        .record_failure(None, "notes", source_key, "write", reason)?;
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
                    continue;
                }
            };
            let current_hash = hash_value(&current);
            if current_hash != base_hash {
                self.store
                    .clear_failure_stage("notes", source_key, "write")?;
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
            match self
                .write_and_verify_notes(local, machine_id, base_hash, &desired)
                .await
            {
                Ok(NotesWriteOutcome::Applied(actual)) => {
                    self.store
                        .clear_failure_stage("notes", source_key, "write")?;
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
                Ok(NotesWriteOutcome::Conflict(actual)) => {
                    self.store
                        .clear_failure_stage("notes", source_key, "write")?;
                    self.cloud
                        .complete_outbound_note(
                            device_id,
                            operation_id,
                            serde_json::json!({
                                "leaseToken": lease_token,
                                "status": "conflict",
                                "actualHash": hash_value(&actual),
                                "actualNotes": actual,
                            }),
                        )
                        .await?;
                }
                Ok(NotesWriteOutcome::Unverified) => {
                    let reason = "GaggiMate did not confirm the Notes update. Sync will retry automatically.";
                    self.store
                        .record_failure(None, "notes", source_key, "write", reason)?;
                    self.cloud
                        .complete_outbound_note(
                            device_id,
                            operation_id,
                            serde_json::json!({
                                "leaseToken": lease_token,
                                "status": "failed",
                                "error": reason,
                            }),
                        )
                        .await?;
                }
                Err(error) => {
                    let reason = "GaggiMate could not confirm the Notes update. Sync will retry automatically.";
                    self.store
                        .record_failure(None, "notes", source_key, "write", reason)?;
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

    async fn execute_profile_store_operation(
        &self,
        local: &GaggiMateClient,
        operation_type: &str,
        payload: &Value,
    ) -> Result<Value, (&'static str, String)> {
        match operation_type {
            "profile_inventory" => {
                let profiles = local
                    .profile_inventory()
                    .await
                    .map_err(|error| ("GAGGIMATE_UNREACHABLE", error.to_string()))?;
                Ok(json!({ "profiles": profiles }))
            }
            "profile_fetch" => {
                let ids = payload.get("profileIds").and_then(Value::as_array).ok_or((
                    "INVALID_OPERATION",
                    "The requested profile selection is invalid".into(),
                ))?;
                if ids.is_empty() || ids.len() > 24 {
                    return Err((
                        "INVALID_OPERATION",
                        "Choose between one and 24 profiles".into(),
                    ));
                }
                let mut profiles = Vec::with_capacity(ids.len());
                for id in ids {
                    let id = id.as_str().ok_or((
                        "INVALID_OPERATION",
                        "A requested profile ID is invalid".into(),
                    ))?;
                    profiles.push(
                        local
                            .load_profile(id)
                            .await
                            .map_err(|error| ("PROFILE_LOAD_FAILED", error.to_string()))?,
                    );
                }
                Ok(json!({ "profiles": profiles }))
            }
            "profile_install_preview" => {
                let profile = payload
                    .get("profile")
                    .ok_or(("INVALID_OPERATION", "The Store profile is missing".into()))?;
                let profile_id = profile.get("id").and_then(Value::as_str).ok_or((
                    "INVALID_OPERATION",
                    "The Store profile ID is missing".into(),
                ))?;
                let inventory = local
                    .profile_inventory()
                    .await
                    .map_err(|error| ("GAGGIMATE_UNREACHABLE", error.to_string()))?;
                let favorite_count = inventory
                    .iter()
                    .filter(|item| {
                        item.get("favorite")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .count();
                let collision = if inventory
                    .iter()
                    .any(|item| item.get("id").and_then(Value::as_str) == Some(profile_id))
                {
                    let existing = local
                        .load_profile(profile_id)
                        .await
                        .map_err(|error| ("PROFILE_LOAD_FAILED", error.to_string()))?;
                    if profiles_equal(&existing, profile) {
                        "identical"
                    } else {
                        "different"
                    }
                } else {
                    "none"
                };
                Ok(json!({
                    "collision": collision,
                    "favoriteCount": favorite_count,
                    "profileId": profile_id,
                }))
            }
            "profile_install" => {
                let profile = payload
                    .get("profile")
                    .ok_or(("INVALID_OPERATION", "The Store profile is missing".into()))?;
                let profile_id = profile.get("id").and_then(Value::as_str).ok_or((
                    "INVALID_OPERATION",
                    "The Store profile ID is missing".into(),
                ))?;
                let actions_only = payload
                    .get("actionsOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let favorite = payload
                    .get("favorite")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || payload
                        .get("selected")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                let selected = payload
                    .get("selected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let expected_collision = payload
                    .get("expectedCollision")
                    .and_then(Value::as_str)
                    .unwrap_or("none");
                let inventory = local
                    .profile_inventory()
                    .await
                    .map_err(|error| ("GAGGIMATE_UNREACHABLE", error.to_string()))?;
                let current = if inventory
                    .iter()
                    .any(|item| item.get("id").and_then(Value::as_str) == Some(profile_id))
                {
                    Some(
                        local
                            .load_profile(profile_id)
                            .await
                            .map_err(|error| ("PROFILE_LOAD_FAILED", error.to_string()))?,
                    )
                } else {
                    None
                };
                let actual_collision = match current.as_ref() {
                    None => "none",
                    Some(existing) if profiles_equal(existing, profile) => "identical",
                    Some(_) => "different",
                };
                if !actions_only && actual_collision != expected_collision {
                    return Err(("PROFILE_CHANGED", "The local profile changed after confirmation; review the installation again".into()));
                }
                let already_installed = actual_collision == "identical";
                if actions_only && !already_installed {
                    return Err((
                        "PROFILE_CHANGED",
                        "The installed profile changed before its machine actions could be applied"
                            .into(),
                    ));
                }
                let installed_profile_id = if !actions_only && !already_installed {
                    let saved_id = local
                        .save_profile(profile)
                        .await
                        .map_err(|error| ("PROFILE_SAVE_FAILED", error.to_string()))?;
                    match local.load_profile(&saved_id).await {
                        Ok(confirmed)
                            if confirmed.get("id").and_then(Value::as_str)
                                == Some(saved_id.as_str()) =>
                        {
                            saved_id
                        }
                        Ok(_) => {
                            return Err((
                                "SAVE_NOT_CONFIRMED",
                                "The machine did not confirm the installed profile".into(),
                            ));
                        }
                        // A save acknowledgement has already changed the machine. Keep the
                        // operation leased and re-check it from the durable Bridge outbox instead
                        // of reporting a false install failure when that immediate reload fails.
                        Err(_) => {
                            return Err(("SAVE_CONFIRMATION_PENDING", saved_id));
                        }
                    }
                } else {
                    profile_id.to_string()
                };
                let mut action_failures = Vec::new();
                let mut favorite_applied = false;
                let mut selected_applied = false;
                if favorite {
                    match local.favorite_profile(&installed_profile_id).await {
                        Ok(()) => favorite_applied = true,
                        Err(_) => action_failures.push("favorite"),
                    }
                }
                if selected {
                    match local.select_profile(&installed_profile_id).await {
                        Ok(()) => selected_applied = true,
                        Err(_) => action_failures.push("select"),
                    }
                }
                let final_inventory = local.profile_inventory().await.unwrap_or(inventory);
                let favorite_count = final_inventory
                    .iter()
                    .filter(|item| {
                        item.get("favorite")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .count();
                Ok(json!({
                    "installed": true,
                    "alreadyInstalled": already_installed,
                    "profileId": installed_profile_id,
                    "favoriteApplied": favorite_applied,
                    "selectedApplied": selected_applied,
                    "favoriteCount": favorite_count,
                    "actionFailures": action_failures,
                }))
            }
            _ => Err((
                "UNSUPPORTED_OPERATION",
                "This Profile Store operation is not supported".into(),
            )),
        }
    }

    async fn flush_profile_store_completions(
        &self,
        local: &GaggiMateClient,
        device_id: &str,
    ) -> Result<(), EngineError> {
        for completion in self.store.bridge_completions(8)? {
            let payload = if completion
                .payload
                .get("profileStoreConfirmationPending")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let Some(profile_id) = completion
                    .payload
                    .get("profileId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    self.store
                        .remove_bridge_completion(&completion.operation_id)?;
                    continue;
                };
                let confirmed = match local.load_profile(profile_id).await {
                    Ok(value) => value,
                    // This is deliberately not terminal. The save was acknowledged already, so
                    // a slow or briefly unreachable machine must remain "confirming".
                    Err(_) => continue,
                };
                let completed = if confirmed.get("id").and_then(Value::as_str) == Some(profile_id) {
                    let favorite = completion
                        .payload
                        .get("favorite")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let selected = completion
                        .payload
                        .get("selected")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let mut action_failures = Vec::new();
                    let mut favorite_applied = false;
                    let mut selected_applied = false;
                    if favorite {
                        match local.favorite_profile(profile_id).await {
                            Ok(()) => favorite_applied = true,
                            Err(_) => action_failures.push("favorite"),
                        }
                    }
                    if selected {
                        match local.select_profile(profile_id).await {
                            Ok(()) => selected_applied = true,
                            Err(_) => action_failures.push("select"),
                        }
                    }
                    let favorite_count = local
                        .profile_inventory()
                        .await
                        .unwrap_or_default()
                        .iter()
                        .filter(|item| {
                            item.get("favorite")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                        })
                        .count();
                    json!({
                        "leaseToken": completion.lease_token,
                        "status": "completed",
                        "result": {
                            "installed": true,
                            "alreadyInstalled": false,
                            "profileId": profile_id,
                            "favoriteApplied": favorite_applied,
                            "selectedApplied": selected_applied,
                            "favoriteCount": favorite_count,
                            "actionFailures": action_failures,
                        },
                    })
                } else {
                    json!({
                        "leaseToken": completion.lease_token,
                        "status": "failed",
                        "errorCode": "SAVE_NOT_CONFIRMED",
                        "errorMessage": "The machine did not confirm the installed profile",
                    })
                };
                self.store.queue_bridge_completion(
                    &completion.operation_id,
                    &completion.lease_token,
                    &completed,
                )?;
                completed
            } else {
                completion.payload.clone()
            };
            match self
                .cloud
                .complete_profile_store_operation(device_id, &completion.operation_id, payload)
                .await
            {
                Ok(_) => self
                    .store
                    .remove_bridge_completion(&completion.operation_id)?,
                Err(CloudError::Unreachable) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    async fn process_profile_store_operations(
        &self,
        local: &GaggiMateClient,
        device_id: &str,
        wait_seconds: u8,
    ) -> Result<(), EngineError> {
        {
            let _guard = self.profile_store_lock.lock().await;
            self.flush_profile_store_completions(local, device_id)
                .await?;
        }
        let claim = self
            .cloud
            .claim_profile_store_operations(device_id, wait_seconds)
            .await?;
        let operations = claim
            .get("operations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if operations.is_empty() {
            return Ok(());
        }
        let _guard = self.profile_store_lock.lock().await;
        self.flush_profile_store_completions(local, device_id)
            .await?;
        for operation in &operations {
            let Some(operation_id) = operation.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(operation_type) = operation.get("type").and_then(Value::as_str) else {
                continue;
            };
            let Some(lease_token) = operation.get("leaseToken").and_then(Value::as_str) else {
                continue;
            };
            let payload = operation
                .get("payload")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let completion = match self
                .execute_profile_store_operation(local, operation_type, &payload)
                .await
            {
                Ok(result) => json!({
                    "leaseToken": lease_token,
                    "status": "completed",
                    "result": result,
                }),
                Err((code, message)) => json!({
                    "leaseToken": lease_token,
                    "status": "failed",
                    "errorCode": code,
                    "errorMessage": message,
                }),
            };
            let completion = if completion.get("errorCode").and_then(Value::as_str)
                == Some("SAVE_CONFIRMATION_PENDING")
            {
                json!({
                    "profileStoreConfirmationPending": true,
                    "profileId": completion.get("errorMessage").and_then(Value::as_str).unwrap_or_default(),
                    "favorite": payload.get("favorite").and_then(Value::as_bool).unwrap_or(false),
                    "selected": payload.get("selected").and_then(Value::as_bool).unwrap_or(false),
                })
            } else {
                completion
            };
            self.store
                .queue_bridge_completion(operation_id, lease_token, &completion)?;
            self.flush_profile_store_completions(local, device_id)
                .await?;
        }
        Ok(())
    }

    pub async fn wait_for_profile_store_operations(&self) -> Result<(), EngineError> {
        let device_id = self
            .store
            .setting("device_id")?
            .ok_or(CloudError::Revoked)?;
        let host = self
            .store
            .setting("machine_host")?
            .unwrap_or_else(|| "gaggimate.local".into());
        let local = GaggiMateClient::new(&host)?;
        self.process_profile_store_operations(&local, &device_id, 25)
            .await
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
            let local = GaggiMateClient::new(&host)?;
            self.process_profile_store_operations(&local, &device_id, 0)
                .await?;
            if !configured {
                self.cloud.heartbeat(&device_id, true, None, None).await?;
                let mut status = self.status.write().await;
                status.machine_reachable = true;
                return Ok(());
            }
            // The Store bridge and the regular synchronizer both speak the
            // GaggiMate WebSocket protocol. Keep their machine requests
            // serialized even though their cloud work is independent.
            let skipped = {
                let _machine_guard = self.profile_store_lock.lock().await;
                self.queue_local_changes(&local, &state).await?
            };
            if cloud_unreachable {
                // Local changes are safely queued before reporting the missing
                // internet connection. They are uploaded on the next cycle.
                return Err(CloudError::Unreachable.into());
            }
            let invalid = self.flush_queue(&device_id).await?;
            {
                let _machine_guard = self.profile_store_lock.lock().await;
                self.process_outbound_notes(&local, &device_id).await?;
            }
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
                self.credentials.delete_tokens()?;
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
        let credentials_removed = self.credentials.delete_tokens().is_ok();
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
}

fn diagnostic_guidance(status: &AppStatus, pending: usize, failures: usize) -> Vec<Value> {
    let mut guidance = Vec::new();
    if !status.connected {
        guidance.push(json!({
            "code": "ACCOUNT_NOT_CONNECTED",
            "message": "Connect this installation in a browser before it can synchronize.",
            "nextCommand": "auth begin",
        }));
    }
    if status.connected && !status.machine_reachable {
        guidance.push(json!({
            "code": "GAGGIMATE_UNREACHABLE",
            "message": format!("GaggiMate at {} is not currently reachable. Check the hostname, IP address, and Docker network.", status.machine_host),
            "nextCommand": format!("host set {}", status.machine_host),
        }));
    }
    if pending > 0 {
        guidance.push(json!({
            "code": "PENDING_UPLOADS",
            "message": format!("{pending} local item(s) are waiting in the encrypted offline queue."),
            "nextCommand": "sync-once",
        }));
    }
    if failures > 0 {
        guidance.push(json!({
            "code": "SYNC_FAILURES",
            "message": format!("{failures} item(s) need another synchronization attempt."),
            "nextCommand": "retry",
        }));
    }
    if status.suppressed > 0 && status.duplicate_policy == "reuse_matching" {
        guidance.push(json!({
            "code": "MATCHING_ITEMS_REUSED",
            "message": format!("{} matching library item(s) were protected from duplicate import by the reuse_matching policy. Nothing was restored or imported automatically.", status.suppressed),
            "nextCommand": "resync preview",
        }));
    }
    if status.conflicts > 0 {
        guidance.push(json!({
            "code": "SYNC_CONFLICTS",
            "message": format!("{} conflict(s) need review in MyBrewFolio Sync settings before making a change.", status.conflicts),
        }));
    }
    if guidance.is_empty() {
        guidance.push(json!({
            "code": "SYNC_HEALTHY",
            "message": "The local queue is empty and no synchronization problems are currently reported.",
        }));
    }
    guidance
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use super::{
        api_timestamp, batch_result_status, diagnostic_guidance, hash_value,
        is_terminal_batch_status, normalized_notes, notes_are_semantically_empty, profiles_equal,
        scan_due, select_sync_batch, serialized_batch_bytes, shot_fingerprint, shot_read_failure,
        shot_source_key, should_refresh_notes, suppressed_items, NotesWriteOutcome, SyncEngine,
        MAX_SYNC_BATCH_BYTES, NOTES_WRITE_ATTEMPTS,
    };
    use crate::model::{AppStatus, IndexEntry, OAuthTokens, SyncObject};
    use crate::{
        cloud::CloudConfig,
        credentials::EncryptedFileCredentialStore,
        local::{GaggiMateClient, LocalError},
        store::AppStore,
    };
    use chrono::{Duration, TimeZone, Utc};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    #[derive(Clone)]
    struct NotesFixtureState {
        current: Value,
        pending: Option<Value>,
        stale_reads_remaining: usize,
        ignore_writes: bool,
        external_change_after_first_write: Option<Value>,
        writes: usize,
    }

    async fn notes_write_server(
        initial: Value,
        stale_reads: usize,
        ignore_writes: bool,
        external_change_after_first_write: Option<Value>,
    ) -> (String, Arc<StdMutex<NotesFixtureState>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("GaggiMate Notes listener");
        let address = listener.local_addr().expect("GaggiMate Notes address");
        let state = Arc::new(StdMutex::new(NotesFixtureState {
            current: initial,
            pending: None,
            stale_reads_remaining: stale_reads,
            ignore_writes,
            external_change_after_first_write,
            writes: 0,
        }));
        let server_state = state.clone();
        tokio::spawn(async move {
            for _ in 0..16 {
                let (stream, _) = listener.accept().await.expect("GaggiMate Notes request");
                let mut socket = accept_async(stream)
                    .await
                    .expect("GaggiMate Notes WebSocket");
                let request = socket
                    .next()
                    .await
                    .expect("GaggiMate Notes message")
                    .expect("valid GaggiMate Notes message")
                    .into_text()
                    .expect("text GaggiMate Notes message");
                let request: Value = serde_json::from_str(&request).expect("GaggiMate Notes JSON");
                let rid = request["rid"].as_str().expect("GaggiMate Notes request ID");
                let response = match request["tp"].as_str() {
                    Some("req:history:notes:get") => {
                        let mut state = server_state.lock().expect("Notes state");
                        if state.pending.is_some() && state.stale_reads_remaining == 0 {
                            state.current = state.pending.take().expect("pending Notes");
                        } else if state.pending.is_some() {
                            state.stale_reads_remaining -= 1;
                        }
                        json!({
                            "tp": "res:history:notes:get", "rid": rid,
                            "notes": state.current,
                        })
                    }
                    Some("req:history:notes:save") => {
                        let mut state = server_state.lock().expect("Notes state");
                        state.writes += 1;
                        if state.writes == 1 {
                            if let Some(external) = state.external_change_after_first_write.take() {
                                state.current = external;
                                state.pending = None;
                            } else if !state.ignore_writes {
                                state.pending = Some(request["notes"].clone());
                            }
                        } else if !state.ignore_writes {
                            state.pending = Some(request["notes"].clone());
                        }
                        json!({ "tp": "res:history:notes:save", "rid": rid })
                    }
                    _ => json!({ "tp": "res:error", "rid": rid, "error": "unexpected request" }),
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .expect("GaggiMate Notes response");
            }
        });
        (format!("127.0.0.1:{}", address.port()), state)
    }

    fn status() -> AppStatus {
        AppStatus {
            connected: true,
            machine_host: "gaggimate.local".into(),
            machine_reachable: true,
            syncing: false,
            last_sync_at: None,
            last_error: None,
            profiles: 0,
            shots: 10,
            notes: 0,
            conflicts: 0,
            suppressed: 350,
            initial_sync_configured: true,
            duplicate_policy: "reuse_matching".into(),
            notes_sync_status: "one_way".into(),
            notes_sync_target_device_id: None,
            notes_sync_writer_device_id: None,
            this_device_id: None,
            notes_sync_intro_seen: false,
            note_backups: Vec::new(),
            issues: Vec::new(),
        }
    }

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
    fn recognizes_absent_and_default_gaggimate_notes_as_empty() {
        assert!(notes_are_semantically_empty(&json!({})));
        assert!(notes_are_semantically_empty(&json!({
            "id": "1",
            "timestamp": 1_735_689_600,
            "rating": 0,
            "beanType": "",
            "doseIn": "",
            "doseOut": "",
            "ratio": "",
            "grindSetting": "",
            "balanceTaste": "balanced",
            "notes": "",
        })));
        assert!(!notes_are_semantically_empty(
            &json!({ "notes": "Keep this" })
        ));
        assert_eq!(
            normalized_notes(json!({ "id": "1", "rating": 0 })),
            json!({})
        );
    }

    #[tokio::test]
    async fn retries_gaggimate_notes_writes_until_the_target_is_verified() {
        let (engine, _directory) = test_engine();
        let base = json!({ "notes": "before" });
        let desired = json!({ "notes": "after" });
        let (host, state) = notes_write_server(base.clone(), 2, false, None).await;
        let local = GaggiMateClient::new(&host).expect("local client");

        let result = engine
            .write_and_verify_notes(&local, 1, &hash_value(&base), &desired)
            .await
            .expect("write verification succeeds");

        match result {
            NotesWriteOutcome::Applied(actual) => assert_eq!(actual, desired),
            _ => panic!("the delayed write should be verified"),
        }
        assert_eq!(state.lock().expect("Notes state").writes, 3);
    }

    #[tokio::test]
    async fn reports_an_unverified_gaggimate_notes_write_without_false_success() {
        let (engine, _directory) = test_engine();
        let base = json!({ "notes": "before" });
        let desired = json!({ "notes": "after" });
        let (host, state) = notes_write_server(base.clone(), 0, true, None).await;
        let local = GaggiMateClient::new(&host).expect("local client");

        let result = engine
            .write_and_verify_notes(&local, 1, &hash_value(&base), &desired)
            .await
            .expect("unverified write is a handled outcome");

        assert!(matches!(result, NotesWriteOutcome::Unverified));
        assert_eq!(
            state.lock().expect("Notes state").writes,
            NOTES_WRITE_ATTEMPTS
        );
    }

    #[tokio::test]
    async fn stops_retrying_when_notes_change_externally_during_verification() {
        let (engine, _directory) = test_engine();
        let base = json!({ "notes": "before" });
        let desired = json!({ "notes": "from MyBrewFolio" });
        let external = json!({ "notes": "edited on GaggiMate" });
        let (host, state) =
            notes_write_server(base.clone(), 0, false, Some(external.clone())).await;
        let local = GaggiMateClient::new(&host).expect("local client");

        let result = engine
            .write_and_verify_notes(&local, 1, &hash_value(&base), &desired)
            .await
            .expect("external edit is a handled outcome");

        match result {
            NotesWriteOutcome::Conflict(actual) => assert_eq!(actual, external),
            _ => panic!("the external edit must become a conflict"),
        }
        assert_eq!(state.lock().expect("Notes state").writes, 1);
    }

    #[test]
    fn profile_comparison_ignores_only_machine_runtime_selection_state() {
        let installed = json!({
            "id": "profile-1",
            "label": "Flat white",
            "favorite": true,
            "selected": false,
            "phases": [{ "name": "Preinfusion", "temperature": 93 }]
        });
        let store = json!({
            "id": "profile-1",
            "label": "Flat white",
            "favorite": false,
            "selected": true,
            "phases": [{ "name": "Preinfusion", "temperature": 93 }]
        });
        let changed = json!({
            "id": "profile-1",
            "label": "Flat white",
            "phases": [{ "name": "Preinfusion", "temperature": 94 }]
        });

        assert!(profiles_equal(&installed, &store));
        assert!(!profiles_equal(&installed, &changed));
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
    fn unsupported_shot_format_has_an_actionable_failure_message() {
        assert_eq!(
            shot_read_failure(&LocalError::UnsupportedShotFormat(7)),
            "GaggiMate shot format v7 is not supported by this MyBrewFolio Sync version"
        );
        assert_eq!(
            shot_read_failure(&LocalError::InvalidData),
            "Shot could not be read from the GaggiMate"
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

    #[test]
    fn diagnostics_explain_safely_reused_matches() {
        let guidance = diagnostic_guidance(&status(), 0, 0);
        assert_eq!(guidance[0]["code"], "MATCHING_ITEMS_REUSED");
        assert_eq!(guidance[0]["nextCommand"], "resync preview");
        assert!(guidance[0]["message"]
            .as_str()
            .expect("diagnostic message")
            .contains("Nothing was restored or imported automatically"));
    }

    fn sync_object(key: &str, bytes: usize) -> SyncObject {
        SyncObject {
            kind: "shot".into(),
            source_key: key.into(),
            source_hash: "a".repeat(64),
            shot_source_key: None,
            data: json!({ "payload": serde_json::Value::String("x".repeat(bytes)) }),
        }
    }

    #[test]
    fn splits_batches_before_the_api_limit() {
        let pending = vec![
            sync_object("one", 3_800_000),
            sync_object("two", 3_800_000),
            sync_object("three", 100),
        ];

        let (batch, oversized) = select_sync_batch(&pending);

        assert!(oversized.is_none());
        assert_eq!(batch.len(), 1);
        assert!(serialized_batch_bytes(&batch) <= MAX_SYNC_BATCH_BYTES);
    }

    #[test]
    fn identifies_an_object_that_cannot_fit_in_any_batch() {
        let pending = vec![sync_object("too-large", MAX_SYNC_BATCH_BYTES)];

        let (batch, oversized) = select_sync_batch(&pending);

        assert!(batch.is_empty());
        assert_eq!(oversized.expect("oversized object").source_key, "too-large");
    }

    #[test]
    fn identifies_suppressed_items_in_both_json_naming_styles() {
        let suppressed = suppressed_items(&json!({ "items": [
            { "kind": "shot", "sourceKey": "1:100", "suppressed": true },
            { "kind": "notes", "source_key": "1:100", "suppressed": true },
            { "kind": "profile", "sourceKey": "p1", "suppressed": false },
            { "kind": "shot", "sourceKey": "missing" }
        ]}));

        assert!(suppressed.contains(&("shot".into(), "1:100".into())));
        assert!(suppressed.contains(&("notes".into(), "1:100".into())));
        assert_eq!(suppressed.len(), 2);
    }

    #[test]
    fn scan_and_note_refresh_decisions_respect_intervals_and_recent_window() {
        let now = Utc::now();
        assert!(scan_due(now, None, Duration::minutes(5)));
        assert!(scan_due(
            now,
            Some(now - Duration::minutes(6)),
            Duration::minutes(5)
        ));
        assert!(!scan_due(
            now,
            Some(now - Duration::minutes(4)),
            Duration::minutes(5)
        ));
        assert!(should_refresh_notes(false, false, true, 19));
        assert!(!should_refresh_notes(false, false, true, 20));
        assert!(should_refresh_notes(true, false, false, 99));
        assert!(should_refresh_notes(false, true, false, 99));
    }

    #[test]
    fn shot_identity_and_batch_result_helpers_have_stable_defaults() {
        let entry = IndexEntry {
            id: 7,
            timestamp: 123,
            duration: 456,
            volume: Some(18.5),
            rating: Some(4),
            profile_id: "profile".into(),
            profile_name: "Espresso".into(),
            incomplete: false,
        };
        assert_eq!(shot_source_key(&entry), "7:123");
        assert_eq!(shot_fingerprint(&entry), "123:456:18.5:4");
        assert_eq!(
            batch_result_status(&json!({ "index": 2, "status": "created" })),
            (2, "created")
        );
        assert_eq!(batch_result_status(&json!({})), (usize::MAX, "invalid"));
        assert!(is_terminal_batch_status("suppressed"));
        assert!(!is_terminal_batch_status("retry"));
    }

    fn test_engine() -> (SyncEngine, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let key_path = directory.path().join("key");
        std::fs::write(&key_path, [3_u8; 32]).expect("key written");
        let credentials = Arc::new(
            EncryptedFileCredentialStore::from_key_file(
                directory.path().join("credentials.enc"),
                &key_path,
            )
            .expect("credentials store"),
        );
        let store = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).expect("store"));
        (
            SyncEngine::open(store, credentials).expect("engine"),
            directory,
        )
    }

    async fn cloud_server(responses: Vec<&str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("cloud listener");
        let address = listener.local_addr().expect("cloud address");
        let responses = responses
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            for body in responses {
                let (mut stream, _) = listener.accept().await.expect("cloud request");
                let mut request = [0_u8; 8_192];
                let _ = stream.read(&mut request).await.expect("cloud request read");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("cloud response");
            }
        });
        format!("http://{address}")
    }

    fn history_index() -> Vec<u8> {
        let mut bytes = vec![0_u8; 32 + 128];
        bytes[0..4].copy_from_slice(&0x5844_4953_u32.to_le_bytes());
        bytes[6..8].copy_from_slice(&128_u16.to_le_bytes());
        bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
        let entry = 32;
        bytes[entry..entry + 4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[entry + 4..entry + 8].copy_from_slice(&1_735_689_600_u32.to_le_bytes());
        bytes[entry + 8..entry + 12].copy_from_slice(&300_u32.to_le_bytes());
        bytes[entry + 12..entry + 14].copy_from_slice(&180_u16.to_le_bytes());
        bytes[entry + 14] = 4;
        bytes[entry + 15] = 1;
        bytes[entry + 16..entry + 25].copy_from_slice(b"profile-1");
        bytes[entry + 48..entry + 60].copy_from_slice(b"Test profile");
        bytes
    }

    fn history_shot() -> Vec<u8> {
        let mut bytes = vec![0_u8; 512 + 30];
        bytes[0..4].copy_from_slice(&0x544f_4853_u32.to_le_bytes());
        bytes[4] = 7;
        bytes[5] = 30;
        bytes[6..8].copy_from_slice(&512_u16.to_le_bytes());
        bytes[8..10].copy_from_slice(&250_u16.to_le_bytes());
        bytes[12..16].copy_from_slice(&0x3fff_u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&300_u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&1_735_689_600_u32.to_le_bytes());
        bytes[28..37].copy_from_slice(b"profile-1");
        bytes[60..72].copy_from_slice(b"Test profile");
        bytes[108..110].copy_from_slice(&180_u16.to_le_bytes());
        bytes[512..516].copy_from_slice(&300_u32.to_le_bytes());
        let values = [
            930_u16, 925, 20, 18, 180, 200, 170, 0, 180, 180, 0, 0x000d, 123,
        ];
        for (index, value) in values.iter().enumerate() {
            let offset = 516 + index * 2;
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    async fn gaggimate_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("GaggiMate listener");
        let address = listener.local_addr().expect("GaggiMate address");
        tokio::spawn(async move {
            for _ in 0..8 {
                let (mut stream, _) = listener.accept().await.expect("GaggiMate request");
                let mut prefix = [0_u8; 64];
                let length = stream.peek(&mut prefix).await.expect("request preview");
                if String::from_utf8_lossy(&prefix[..length]).starts_with("GET /ws") {
                    let mut socket = accept_async(stream).await.expect("WebSocket accepted");
                    let request = socket
                        .next()
                        .await
                        .expect("WebSocket request")
                        .expect("valid WebSocket message")
                        .into_text()
                        .expect("text WebSocket message");
                    let request: serde_json::Value =
                        serde_json::from_str(&request).expect("JSON WebSocket request");
                    let rid = request["rid"].as_str().expect("request ID");
                    let response = match request["tp"].as_str() {
                        Some("req:profiles:list") => json!({
                            "tp": "res:profiles:list", "rid": rid,
                            "profiles": [{ "id": "profile-1" }]
                        }),
                        Some("req:profiles:load") => json!({
                            "tp": "res:profiles:load", "rid": rid,
                            "profile": { "id": "profile-1", "name": "Test profile" }
                        }),
                        Some("req:history:notes:get") => json!({
                            "tp": "res:history:notes:get", "rid": rid,
                            "notes": { "text": "dial in finer" }
                        }),
                        Some("req:history:notes:save") => json!({
                            "tp": "res:history:notes:save", "rid": rid
                        }),
                        _ => {
                            json!({ "tp": "res:error", "rid": rid, "error": "unexpected request" })
                        }
                    };
                    socket
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .expect("WebSocket response");
                } else {
                    let mut request = [0_u8; 1_024];
                    let length = stream.read(&mut request).await.expect("HTTP request read");
                    let request = String::from_utf8_lossy(&request[..length]);
                    let (content_type, body) = if request.starts_with("GET /api/history/index.bin")
                    {
                        ("application/octet-stream", history_index())
                    } else if request.starts_with("GET /api/history/000001.slog") {
                        ("application/octet-stream", history_shot())
                    } else {
                        ("application/json", b"{}".to_vec())
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("HTTP headers");
                    stream.write_all(&body).await.expect("HTTP body");
                }
            }
        });
        format!("127.0.0.1:{}", address.port())
    }

    fn configure_test_cloud(engine: &mut SyncEngine, api_url: &str) {
        engine.cloud.config = CloudConfig {
            api_url: api_url.into(),
            client_id: "test-client".into(),
            authorize_url: format!("{api_url}/authorize"),
            token_url: format!("{api_url}/token"),
            redirect_uri: "mybrewfolio-sync://oauth/callback".into(),
            device_redirect_uri: format!("{api_url}/callback"),
        };
    }

    fn connect_test_engine(engine: &SyncEngine) {
        engine
            .credentials
            .save_tokens(&OAuthTokens {
                access_token: "valid-access".into(),
                refresh_token: Some("refresh".into()),
                expires_at: i64::MAX,
            })
            .expect("token saved");
        engine
            .store
            .set_setting("device_id", "device-1")
            .expect("device saved");
    }

    #[tokio::test]
    async fn updates_status_counts_and_notes_metadata_from_cloud_state() {
        let (engine, _directory) = test_engine();
        engine
            .update_from_cloud_state(&json!({
                "source": {
                    "initialSyncConfiguredAt": "2026-08-20T10:00:00Z",
                    "duplicatePolicy": "import_all",
                    "notesSyncStatus": "two_way",
                    "notesSyncTargetDeviceId": "target",
                    "notesSyncWriterDeviceId": "writer",
                    "lastSyncAt": "2026-08-20T11:00:00Z"
                },
                "items": [
                    { "kind": "profile", "present": true },
                    { "kind": "shot", "present": true },
                    { "kind": "notes", "present": true },
                    { "kind": "shot", "present": false, "conflict": true },
                    { "kind": "shot", "suppressed": true }
                ],
                "noteBackups": [
                    { "id": "backup", "slot": "latest", "itemCount": 2, "createdAt": "2026-08-20T09:00:00Z", "finalizedAt": null },
                    { "slot": "invalid" }
                ]
            }))
            .await;

        let status = engine.status().await;
        assert_eq!(status.profiles, 1);
        assert_eq!(status.shots, 1);
        assert_eq!(status.notes, 1);
        assert_eq!(status.conflicts, 1);
        assert_eq!(status.suppressed, 1);
        assert!(status.initial_sync_configured);
        assert_eq!(status.duplicate_policy, "import_all");
        assert_eq!(status.notes_sync_status, "two_way");
        assert_eq!(status.note_backups.len(), 1);
        assert_eq!(status.last_sync_at.as_deref(), Some("2026-08-20T11:00:00Z"));
    }

    #[tokio::test]
    async fn diagnose_reports_queue_counts_and_actionable_guidance() {
        let (engine, _directory) = test_engine();
        engine
            .store
            .set_setting("device_id", "device")
            .expect("device saved");
        engine
            .store
            .queue(&sync_object("pending", 10))
            .expect("pending queued");
        engine
            .store
            .record_failure(None, "shot", "failed", "read", "not available")
            .expect("failure recorded");

        let report = engine.diagnose().await.expect("diagnostics");

        assert_eq!(report["queue"]["pending"], 1);
        assert_eq!(report["queue"]["failures"], 1);
        let codes = report["guidance"]
            .as_array()
            .expect("guidance list")
            .iter()
            .filter_map(|item| item["code"].as_str())
            .collect::<Vec<_>>();
        assert!(codes.contains(&"ACCOUNT_NOT_CONNECTED"));
        assert!(codes.contains(&"PENDING_UPLOADS"));
        assert!(codes.contains(&"SYNC_FAILURES"));
    }

    #[tokio::test]
    async fn host_and_local_preferences_round_trip_without_network_access() {
        let (engine, _directory) = test_engine();
        engine.set_host("127.0.0.1:8088").await.expect("host saved");
        assert_eq!(engine.status().await.machine_host, "127.0.0.1:8088");
        assert!(engine.set_host("https://public.example").await.is_err());
        assert!(!engine.hide_app_icon().expect("default icon setting"));
        engine.set_hide_app_icon(true).expect("icon hidden");
        assert!(engine.hide_app_icon().expect("icon setting"));
        engine
            .dismiss_notes_sync_intro()
            .await
            .expect("intro dismissed");
        assert!(engine.status().await.notes_sync_intro_seen);
    }

    #[tokio::test]
    async fn sync_once_reads_gaggimate_queues_all_item_kinds_and_flushes_the_batch() {
        let (mut engine, _directory) = test_engine();
        let api_url = cloud_server(vec![
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z"},"items":[]}"#,
            r#"{"operations":[]}"#,
            r#"{"results":[{"index":0,"status":"created"},{"index":1,"status":"created"},{"index":2,"status":"created"}]}"#,
            r#"{"operations":[]}"#,
            "{}",
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z","duplicatePolicy":"reuse_matching"},"items":[{"kind":"profile"},{"kind":"shot"},{"kind":"notes"}]}"#,
        ])
        .await;
        configure_test_cloud(&mut engine, &api_url);
        connect_test_engine(&engine);
        let host = gaggimate_server().await;
        let local = GaggiMateClient::new(&host).expect("local client");
        let v7_shot = local.shot(1).await.expect("v7 shot reads");
        assert_eq!(v7_shot["samples"][0]["t"], 300.0);
        assert_eq!(v7_shot["samples"][0]["ct"], 92.5);
        assert_eq!(v7_shot["samples"][0]["wp"], 12.3);

        engine
            .store
            .set_setting("machine_host", &host)
            .expect("host saved");

        let skipped = engine
            .queue_local_changes(&local, &json!({ "items": [] }))
            .await
            .expect("v7 shot queues");
        assert!(skipped.is_empty());
        let queued = engine.store.pending(25).expect("queue reads");
        let queued_shot = queued
            .iter()
            .find(|object| object.kind == "shot")
            .expect("shot is queued");
        assert_eq!(queued_shot.data["samples"][0]["wp"], 12.3);

        engine.sync_once().await.expect("sync succeeds");

        let status = engine.status().await;
        assert!(status.machine_reachable);
        assert!(status.last_sync_at.is_some());
        assert_eq!((status.profiles, status.shots, status.notes), (1, 1, 1));
        assert_eq!(engine.store.pending_count().expect("empty queue"), 0);
        assert_eq!(engine.store.failure_count().expect("no failures"), 0);
    }

    #[tokio::test]
    async fn browser_and_device_pairing_register_the_same_connected_installation() {
        let (mut engine, _directory) = test_engine();
        let api_url = cloud_server(vec![
            r#"{"access_token":"browser-access","refresh_token":"browser-refresh","expires_in":3600}"#,
            r#"{"device":{"id":"desktop-device","sourceId":"desktop-source"}}"#,
            r#"{"requestId":"request-1","userCode":"ABCD-1234","verificationUri":"https://example.test/pair","pollToken":"poll-token","expiresIn":600}"#,
            r#"{"status":"authorized","authorizationCode":"device-code"}"#,
            r#"{"access_token":"device-access","refresh_token":"device-refresh","expires_in":3600}"#,
            r#"{"device":{"id":"headless-device","sourceId":"headless-source"}}"#,
        ])
        .await;
        configure_test_cloud(&mut engine, &api_url);

        let browser_url = engine.begin_oauth().await.expect("browser OAuth starts");
        let state = browser_url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("OAuth state");
        engine
            .complete_oauth(&format!(
                "mybrewfolio-sync://oauth/callback?code=browser-code&state={state}"
            ))
            .await
            .expect("browser OAuth completes");
        assert_eq!(
            engine.status().await.this_device_id.as_deref(),
            Some("desktop-device")
        );

        let pairing = engine.begin_device_oauth().await.expect("pairing starts");
        assert_eq!(pairing.user_code, "ABCD-1234");
        assert!(engine.poll_device_oauth().await.expect("pairing completes"));
        assert_eq!(
            engine.status().await.this_device_id.as_deref(),
            Some("headless-device")
        );
        assert!(engine
            .credentials
            .pending_device_authorization()
            .expect("pairing state read")
            .is_none());
    }

    #[tokio::test]
    async fn settings_and_resync_refresh_cloud_state_and_reset_stale_queue_data() {
        let (mut engine, _directory) = test_engine();
        let api_url = cloud_server(vec![
            "{}",
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z","duplicatePolicy":"import_all"},"items":[]}"#,
            r#"{"restored":1,"merged":0}"#,
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z"},"items":[]}"#,
        ])
        .await;
        configure_test_cloud(&mut engine, &api_url);
        connect_test_engine(&engine);
        engine
            .store
            .set_setting("shot_fingerprint_v2:1:2", "old")
            .expect("fingerprint saved");
        engine
            .store
            .queue(&sync_object("pending", 10))
            .expect("pending queued");

        engine.configure_sync(false).await.expect("settings saved");
        assert_eq!(engine.status().await.duplicate_policy, "import_all");
        let applied = engine
            .apply_resync(json!({ "restoreItemIds": ["restore-1"] }))
            .await
            .expect("resync applied");

        assert_eq!(applied["restored"], 1);
        assert_eq!(engine.store.pending_count().expect("queue reset"), 0);
        assert!(engine
            .store
            .setting("shot_fingerprint_v2:1:2")
            .expect("fingerprint read")
            .is_none());
    }

    #[tokio::test]
    async fn notes_backup_and_resync_preview_use_the_current_local_inventory() {
        let (mut engine, _directory) = test_engine();
        let api_url = cloud_server(vec![
            r#"{"backup":{"id":"backup-1"}}"#,
            "{}",
            "{}",
            r#"{"source":{},"items":[]}"#,
            r#"{"restoreItems":[{"kind":"shot","sourceKey":"1:1735689600"}],"duplicates":[]}"#,
        ])
        .await;
        configure_test_cloud(&mut engine, &api_url);
        connect_test_engine(&engine);
        engine
            .store
            .set_setting("machine_host", &gaggimate_server().await)
            .expect("host saved");

        let backup_id = engine
            .create_latest_notes_backup()
            .await
            .expect("backup created");
        let preview = engine.resync_preview().await.expect("resync preview");

        assert_eq!(backup_id, "backup-1");
        assert_eq!(preview["restoreItems"][0]["sourceKey"], "1:1735689600");
    }

    #[tokio::test]
    async fn notes_activation_and_restore_preserve_the_verified_machine_content() {
        let (mut engine, _directory) = test_engine();
        let source_key = "1:1735689600";
        let notes = json!({ "text": "dial in finer" });
        let notes_hash = hash_value(&notes);
        let api_url = cloud_server(vec![
            "{}",
            r#"{"backup":{"id":"activation-backup"}}"#,
            "{}",
            "{}",
            r#"{"items":[{"sourceKey":"1:1735689600","differs":true}]}"#,
        ])
        .await;
        configure_test_cloud(&mut engine, &api_url);
        connect_test_engine(&engine);
        engine
            .store
            .set_setting("machine_host", &gaggimate_server().await)
            .expect("host saved");

        let activation = engine
            .begin_two_way_notes_activation()
            .await
            .expect("activation preview");

        assert_eq!(activation["backupId"], "activation-backup");
        assert!(engine.status().await.notes_sync_intro_seen);

        let (mut restore_engine, _directory) = test_engine();
        let restore_api = cloud_server(vec![
            r#"{"backup":{"id":"latest-backup"}}"#,
            "{}",
            "{}",
            &format!(
                r#"{{"items":[{{"sourceKey":"{source_key}","notes":{notes},"notesHash":"{notes_hash}"}}]}}"#
            ),
            r#"{"applied":1}"#,
        ])
        .await;
        configure_test_cloud(&mut restore_engine, &restore_api);
        connect_test_engine(&restore_engine);
        restore_engine
            .store
            .set_setting("machine_host", &gaggimate_server().await)
            .expect("host saved");

        let restored = restore_engine
            .restore_notes_backup("selected-backup", &[source_key.into()])
            .await
            .expect("notes restored");

        assert_eq!(restored, json!({ "applied": 1, "skipped": 0 }));
    }

    #[tokio::test]
    async fn outbound_notes_report_conflicts_and_successful_machine_writes() {
        let (mut engine, _directory) = test_engine();
        let current_notes = json!({ "text": "dial in finer" });
        let api_url = cloud_server(vec![
            &format!(
                r#"{{"operations":[
                    {{"id":"conflict","leaseToken":"lease-1","sourceKey":"1:1735689600","baseSourceHash":"not-current","desiredNotes":{{}}}},
                    {{"id":"apply","leaseToken":"lease-2","sourceKey":"1:1735689600","baseSourceHash":"{}","desiredNotes":{{"text":"new note"}}}}
                ]}}"#,
                hash_value(&current_notes)
            ),
            "{}",
            "{}",
        ])
        .await;
        configure_test_cloud(&mut engine, &api_url);
        connect_test_engine(&engine);
        let local = GaggiMateClient::new(&gaggimate_server().await).expect("local client");

        engine
            .process_outbound_notes(&local, "device-1")
            .await
            .expect("outbound operations processed");
        let issues = engine.store.failures().expect("write issues");
        assert!(issues.iter().any(|issue| {
            issue.kind == "notes"
                && issue.stage == "write"
                && issue.reason
                    == "GaggiMate did not confirm the Notes update. Sync will retry automatically."
        }));
    }

    #[tokio::test]
    async fn disconnect_always_clears_local_account_data_when_the_server_is_unavailable() {
        let (mut engine, _directory) = test_engine();
        engine.cloud.config.api_url = "http://127.0.0.1:1".into();
        engine
            .store
            .set_setting("device_id", "device-1")
            .expect("device saved");
        engine
            .store
            .set_setting("source_id", "source-1")
            .expect("source saved");
        engine
            .store
            .queue(&sync_object("pending", 10))
            .expect("pending queued");
        engine
            .credentials
            .save_tokens(&OAuthTokens {
                access_token: "valid-access".into(),
                refresh_token: None,
                expires_at: i64::MAX,
            })
            .expect("token saved");

        let result = engine.disconnect().await.expect("disconnect succeeds");

        assert_eq!(result["serverRevoked"], false);
        assert_eq!(result["credentialsRemoved"], true);
        assert!(!engine.status().await.connected);
        assert!(engine
            .store
            .setting("device_id")
            .expect("device read")
            .is_none());
        assert_eq!(engine.store.pending_count().expect("queue read"), 0);
        assert!(engine.credentials.tokens().expect("tokens read").is_none());
    }
}
