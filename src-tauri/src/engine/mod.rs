// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration as StdDuration,
};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

use crate::{
    cloud::{CloudClient, CloudError, PendingOAuth},
    credentials::CredentialStore,
    local::{GaggiMateClient, LocalError},
    model::{AppStatus, IndexEntry, SyncObject, SyncProgress, SyncProgressPhase},
    store::{AppStore, StoreError},
};

mod auth;
mod control;
mod notes;
mod profile_store;
mod state;
mod sync;

pub(super) const MAX_SYNC_BATCH_ITEMS: usize = 25;
// The API accepts 8 MiB batches. Keep a margin so metadata added by a future
// client version cannot turn an otherwise valid queue entry into a rejected
// request.
pub(super) const MAX_SYNC_BATCH_BYTES: usize = 7 * 1024 * 1024;
pub(super) const NOTES_WRITE_ATTEMPTS: usize = 3;
pub(super) const NOTES_WRITE_RETRY_DELAYS: [StdDuration; NOTES_WRITE_ATTEMPTS - 1] = [
    StdDuration::from_millis(250),
    // The third attempt is scheduled 750 ms after the first, not 750 ms
    // after the second.
    StdDuration::from_millis(500),
];
pub(super) const TWO_WAY_NOTES_PROTOCOL_VERSION: &str = "2";
pub(super) const TWO_WAY_NOTES_PROTOCOL_ANNOUNCED_SETTING: &str =
    "two_way_notes_protocol_announced";

pub(super) type ProfileStoreResult<T> = Result<T, (&'static str, String)>;

pub(super) struct ProfileInstallRequest<'a> {
    profile: &'a Value,
    profile_id: &'a str,
    actions_only: bool,
    favorite: bool,
    selected: bool,
    expected_collision: &'a str,
}

pub(super) fn serialized_batch_bytes(items: &[SyncObject]) -> usize {
    serde_json::to_vec(&serde_json::json!({ "items": items }))
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

pub(super) fn select_sync_batch(pending: &[SyncObject]) -> (Vec<SyncObject>, Option<SyncObject>) {
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

pub(super) fn suppressed_items(cloud_state: &Value) -> HashSet<(String, String)> {
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

pub(super) fn two_way_notes_active(cloud_state: &Value) -> bool {
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

pub(super) fn zero_notes_value(value: &Value) -> bool {
    value.as_f64().is_some_and(|number| number == 0.0)
        || value
            .as_str()
            .is_some_and(|text| matches!(text.trim(), "" | "0" | "0.0"))
}

/// GaggiMate can persist an untouched Notes form as a fully populated default
/// object. It has the same meaning as an absent Notes record and must never
/// silently clear a cloud Note.
pub(super) fn notes_are_semantically_empty(notes: &Value) -> bool {
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

pub(super) fn normalized_notes(notes: Value) -> Value {
    if notes_are_semantically_empty(&notes) {
        json!({})
    } else {
        notes
    }
}

pub(super) enum NotesWriteOutcome {
    Applied(Value),
    Conflict(Value),
    Unverified,
}

pub(super) fn scan_due(
    now: DateTime<Utc>,
    last: Option<DateTime<Utc>>,
    interval: Duration,
) -> bool {
    last.is_none_or(|last| now - last >= interval)
}

pub(super) fn shot_source_key(entry: &IndexEntry) -> String {
    format!("{}:{}", entry.id, entry.timestamp)
}

pub(super) fn shot_fingerprint(entry: &IndexEntry) -> String {
    format!(
        "{}:{}:{}:{}",
        entry.timestamp,
        entry.duration,
        entry.volume.unwrap_or_default(),
        entry.rating.unwrap_or_default()
    )
}

pub(super) fn should_refresh_notes(
    changed: bool,
    full_scan: bool,
    recent_scan: bool,
    position: usize,
) -> bool {
    changed || full_scan || (recent_scan && position < 20)
}

pub(super) fn shot_read_failure(error: &LocalError) -> String {
    match error {
        LocalError::UnsupportedShotFormat(version) => {
            format!(
                "GaggiMate shot format v{version} is not supported by this MyBrewFolio Sync version. Update MyBrewFolio Sync before retrying this shot."
            )
        }
        _ => error.to_string(),
    }
}

pub(super) fn batch_result_status(result: &Value) -> (usize, &str) {
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

pub(super) fn is_terminal_batch_status(status: &str) -> bool {
    matches!(
        status,
        "created" | "updated" | "linked" | "unchanged" | "suppressed" | "conflict" | "invalid"
    )
}

pub(super) fn normalized_profile(value: &Value) -> Value {
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

pub(super) fn profiles_equal(left: &Value, right: &Value) -> bool {
    normalized_profile(left) == normalized_profile(right)
}

pub(super) fn requested_profile(payload: &Value) -> ProfileStoreResult<(&Value, &str)> {
    let profile = payload
        .get("profile")
        .ok_or(("INVALID_OPERATION", "The Store profile is missing".into()))?;
    let profile_id = profile.get("id").and_then(Value::as_str).ok_or((
        "INVALID_OPERATION",
        "The Store profile ID is missing".into(),
    ))?;
    Ok((profile, profile_id))
}

pub(super) fn profile_store_issue_source_key(payload: &Value) -> String {
    payload
        .get("profile")
        .and_then(|profile| profile.get("id"))
        .or_else(|| {
            payload
                .get("profileIds")
                .and_then(Value::as_array)
                .and_then(|ids| ids.first())
        })
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .unwrap_or("profile-store")
        .to_string()
}

pub(super) fn profile_store_public_error_message(code: &str, message: &str) -> String {
    match code {
        "GAGGIMATE_UNREACHABLE" => {
            "GaggiMate could not be reached while completing this profile operation. Check local Sync diagnostics and try again.".into()
        }
        "GAGGIMATE_DATA_INVALID"
        | "GAGGIMATE_HOST_INVALID"
        | "GAGGIMATE_SHOT_FORMAT_UNSUPPORTED"
        | "PROFILE_LOAD_FAILED"
        | "PROFILE_SAVE_FAILED" => {
            "GaggiMate could not complete this profile operation. Check local Sync diagnostics and try again.".into()
        }
        _ => message.to_string(),
    }
}

pub(super) fn profile_store_local_error(error: LocalError) -> (&'static str, String) {
    let message = error.to_string();
    let code = match &error {
        LocalError::Unreachable => "GAGGIMATE_UNREACHABLE",
        LocalError::InvalidHost => "GAGGIMATE_HOST_INVALID",
        LocalError::UnsupportedShotFormat(_) => "GAGGIMATE_SHOT_FORMAT_UNSUPPORTED",
        LocalError::InvalidData
        | LocalError::InvalidHistoryIndex
        | LocalError::InvalidShot(_)
        | LocalError::InvalidNotes(_)
        | LocalError::InvalidProfileList
        | LocalError::InvalidProfile(_)
        | LocalError::InvalidProfileUpdate(_) => "GAGGIMATE_DATA_INVALID",
    };
    (code, message)
}

pub(super) fn profile_install_request(
    payload: &Value,
) -> ProfileStoreResult<ProfileInstallRequest<'_>> {
    let (profile, profile_id) = requested_profile(payload)?;
    let selected = payload
        .get("selected")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ProfileInstallRequest {
        profile,
        profile_id,
        actions_only: payload
            .get("actionsOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        favorite: payload
            .get("favorite")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || selected,
        selected,
        expected_collision: payload
            .get("expectedCollision")
            .and_then(Value::as_str)
            .unwrap_or("none"),
    })
}

pub(super) fn profile_favorite_count(inventory: &[Value]) -> usize {
    inventory
        .iter()
        .filter(|item| {
            item.get("favorite")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count()
}

pub(super) async fn profile_collision(
    local: &GaggiMateClient,
    inventory: &[Value],
    profile_id: &str,
    profile: &Value,
) -> ProfileStoreResult<&'static str> {
    if !inventory
        .iter()
        .any(|item| item.get("id").and_then(Value::as_str) == Some(profile_id))
    {
        return Ok("none");
    }
    let existing = local
        .load_profile(profile_id)
        .await
        .map_err(|error| ("PROFILE_LOAD_FAILED", error.to_string()))?;
    Ok(if profiles_equal(&existing, profile) {
        "identical"
    } else {
        "different"
    })
}

pub(super) fn validate_profile_install(
    request: &ProfileInstallRequest<'_>,
    actual_collision: &str,
) -> ProfileStoreResult<bool> {
    if !request.actions_only && actual_collision != request.expected_collision {
        return Err((
            "PROFILE_CHANGED",
            "The local profile changed after confirmation; review the installation again".into(),
        ));
    }
    let already_installed = actual_collision == "identical";
    if request.actions_only && !already_installed {
        return Err((
            "PROFILE_CHANGED",
            "The installed profile changed before its machine actions could be applied".into(),
        ));
    }
    Ok(already_installed)
}

pub(super) async fn save_profile_if_needed(
    local: &GaggiMateClient,
    request: &ProfileInstallRequest<'_>,
    already_installed: bool,
) -> ProfileStoreResult<String> {
    if request.actions_only || already_installed {
        return Ok(request.profile_id.to_string());
    }
    let saved_id = local
        .save_profile(request.profile)
        .await
        .map_err(|error| ("PROFILE_SAVE_FAILED", error.to_string()))?;
    match local.load_profile(&saved_id).await {
        Ok(confirmed) if confirmed.get("id").and_then(Value::as_str) == Some(saved_id.as_str()) => {
            Ok(saved_id)
        }
        Ok(_) => Err((
            "SAVE_NOT_CONFIRMED",
            "The machine did not confirm the installed profile".into(),
        )),
        // A save acknowledgement has already changed the machine. Keep the operation leased and
        // re-check it from the durable Bridge outbox instead of reporting a false install failure.
        Err(_) => Err(("SAVE_CONFIRMATION_PENDING", saved_id)),
    }
}

pub(super) async fn apply_profile_actions(
    local: &GaggiMateClient,
    profile_id: &str,
    favorite: bool,
    selected: bool,
) -> (Vec<&'static str>, bool, bool) {
    let mut action_failures = Vec::new();
    let favorite_applied = if favorite {
        local.favorite_profile(profile_id).await.is_ok()
    } else {
        false
    };
    if favorite && !favorite_applied {
        action_failures.push("favorite");
    }
    let selected_applied = if selected {
        local.select_profile(profile_id).await.is_ok()
    } else {
        false
    };
    if selected && !selected_applied {
        action_failures.push("select");
    }
    (action_failures, favorite_applied, selected_applied)
}

pub(super) async fn profile_inventory_operation(
    local: &GaggiMateClient,
) -> ProfileStoreResult<Value> {
    let profiles = local
        .profile_inventory()
        .await
        .map_err(profile_store_local_error)?;
    Ok(json!({ "profiles": profiles }))
}

pub(super) async fn profile_fetch_operation(
    local: &GaggiMateClient,
    payload: &Value,
) -> ProfileStoreResult<Value> {
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

pub(super) async fn profile_install_preview_operation(
    local: &GaggiMateClient,
    payload: &Value,
) -> ProfileStoreResult<Value> {
    let (profile, profile_id) = requested_profile(payload)?;
    let inventory = local
        .profile_inventory()
        .await
        .map_err(profile_store_local_error)?;
    let collision = profile_collision(local, &inventory, profile_id, profile).await?;
    Ok(json!({
        "collision": collision,
        "favoriteCount": profile_favorite_count(&inventory),
        "profileId": profile_id,
    }))
}

pub(super) async fn profile_install_operation(
    local: &GaggiMateClient,
    payload: &Value,
) -> ProfileStoreResult<Value> {
    let request = profile_install_request(payload)?;
    let inventory = local
        .profile_inventory()
        .await
        .map_err(profile_store_local_error)?;
    let collision =
        profile_collision(local, &inventory, request.profile_id, request.profile).await?;
    let already_installed = validate_profile_install(&request, collision)?;
    let installed_profile_id = save_profile_if_needed(local, &request, already_installed).await?;
    let (action_failures, favorite_applied, selected_applied) = apply_profile_actions(
        local,
        &installed_profile_id,
        request.favorite,
        request.selected,
    )
    .await;
    let final_inventory = local.profile_inventory().await.unwrap_or(inventory);
    Ok(json!({
        "installed": true,
        "alreadyInstalled": already_installed,
        "profileId": installed_profile_id,
        "favoriteApplied": favorite_applied,
        "selectedApplied": selected_applied,
        "favoriteCount": profile_favorite_count(&final_inventory),
        "actionFailures": action_failures,
    }))
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
    pub(super) fn heartbeat_code(&self) -> &'static str {
        match self {
            Self::Cloud(CloudError::NotConfigured) => "SYNC_OAUTH_NOT_CONFIGURED",
            Self::Cloud(CloudError::OAuth) => "SYNC_OAUTH_FAILED",
            Self::Cloud(CloudError::DeviceAuthorizationRejected) => "SYNC_DEVICE_AUTH_REJECTED",
            Self::Cloud(CloudError::DeviceAuthorizationExchangeFailed) => {
                "SYNC_DEVICE_AUTH_EXCHANGE_FAILED"
            }
            Self::Cloud(CloudError::Revoked) => "SYNC_DEVICE_REVOKED",
            Self::Cloud(CloudError::Unreachable) => "MYBREWFOLIO_UNREACHABLE",
            Self::Cloud(CloudError::Rejected) => "SYNC_DATA_REJECTED",
            Self::Local(LocalError::InvalidHost) => "GAGGIMATE_HOST_INVALID",
            Self::Local(LocalError::Unreachable) => "GAGGIMATE_UNREACHABLE",
            Self::Local(
                LocalError::InvalidData
                | LocalError::InvalidHistoryIndex
                | LocalError::InvalidShot(_)
                | LocalError::InvalidNotes(_)
                | LocalError::InvalidProfileList
                | LocalError::InvalidProfile(_)
                | LocalError::InvalidProfileUpdate(_),
            ) => "GAGGIMATE_DATA_INVALID",
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

    pub(super) fn machine_reachable(&self) -> bool {
        !matches!(
            self,
            Self::Local(LocalError::InvalidHost | LocalError::Unreachable)
        )
    }
}

pub struct SyncEngine {
    store: Arc<AppStore>,
    cloud: CloudClient,
    credentials: Arc<dyn CredentialStore>,
    pending_oauth: Mutex<Option<PendingOAuth>>,
    status: RwLock<AppStatus>,
    control_lock: Mutex<()>,
    sync_lock: Mutex<()>,
    profile_store_lock: Mutex<()>,
}

pub(super) fn canonical_json(value: &Value) -> String {
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
            entries.sort_by_key(|(key, _)| *key);
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

pub(super) fn hash_value(value: &Value) -> String {
    hex_digest(canonical_json(value).as_bytes())
}

pub(super) fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn platform() -> &'static str {
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

pub(super) fn installation_name() -> &'static str {
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

pub(super) fn parse_time(value: Option<String>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc))
}

pub(super) fn api_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(super) fn sync_progress_log_line(progress: &SyncProgress) -> String {
    match progress.phase {
        SyncProgressPhase::ReadingHistory => format!(
            "First sync: Reading history: {} of {} brews",
            progress.scanned_shots, progress.total_shots
        ),
        SyncProgressPhase::Uploading => format!(
            "First sync: Uploading: {} of {} items",
            progress.uploaded_items.unwrap_or_default(),
            progress.total_items.unwrap_or_default()
        ),
        SyncProgressPhase::Finishing => "First sync: Finishing your first sync…".into(),
    }
}

fn gaggimate_unreachable_guidance(host: &str) -> Value {
    let message = if host.eq_ignore_ascii_case("gaggimate.local") {
        "GaggiMate could not be reached. Docker and NAS containers may not resolve gaggimate.local. Set MYBREWFOLIO_SYNC_GAGGIMATE_HOST to GaggiMate's private LAN IP, then recreate the Sync container."
    } else {
        "GaggiMate could not be reached through the configured private LAN address. Check that GaggiMate is online and reachable from the Docker or NAS network."
    };
    json!({
        "code": "GAGGIMATE_UNREACHABLE",
        "message": message,
        "nextCommand": format!("host set {host}"),
    })
}

fn local_error_guidance(status: &AppStatus) -> Option<Value> {
    let code = status.last_error_code.as_deref()?;
    match code {
        "GAGGIMATE_UNREACHABLE" => Some(gaggimate_unreachable_guidance(&status.machine_host)),
        "GAGGIMATE_HOST_INVALID" => Some(json!({
            "code": code,
            "message": "The configured GaggiMate address is invalid. Use gaggimate.local or a private LAN IP address.",
            "nextCommand": "host set <private-lan-ip>",
        })),
        "GAGGIMATE_DATA_INVALID" => Some(json!({
            "code": code,
            "message": status.last_error.as_deref().unwrap_or("GaggiMate returned invalid data. Retry the synchronization."),
            "nextCommand": "sync-once",
        })),
        "GAGGIMATE_SHOT_FORMAT_UNSUPPORTED" => Some(json!({
            "code": code,
            "message": status.last_error.as_deref().unwrap_or("This GaggiMate shot format needs a newer MyBrewFolio Sync version."),
        })),
        _ => None,
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
    if status.connected {
        if let Some(local_error) = local_error_guidance(status) {
            guidance.push(local_error);
        } else if !status.machine_reachable {
            guidance.push(gaggimate_unreachable_guidance(&status.machine_host));
        }
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
mod tests;
