use std::{
    collections::HashSet,
    time::{Duration as StdDuration, Instant},
};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use crate::{
    cloud::CloudError,
    local::{GaggiMateClient, LocalError},
    model::{AppStatus, SyncObject, SyncProgress, SyncProgressPhase},
};

use super::{
    api_timestamp, batch_result_status, hash_value, is_terminal_batch_status, normalized_notes,
    notes_are_semantically_empty, parse_time, scan_due, select_sync_batch, shot_fingerprint,
    shot_read_failure, shot_source_key, should_refresh_notes, suppressed_items,
    two_way_notes_active, EngineError, NotesWriteOutcome, SyncEngine, MAX_SYNC_BATCH_ITEMS,
    NOTES_WRITE_ATTEMPTS, NOTES_WRITE_RETRY_DELAYS, TWO_WAY_NOTES_PROTOCOL_ANNOUNCED_SETTING,
    TWO_WAY_NOTES_PROTOCOL_VERSION,
};

impl SyncEngine {
    #[cfg(test)]
    pub(super) async fn queue_local_changes(
        &self,
        local: &GaggiMateClient,
        cloud_state: &Value,
    ) -> Result<Vec<String>, EngineError> {
        self.queue_local_changes_with_progress(local, cloud_state, None)
            .await
            .map(|(skipped, _)| skipped)
    }

    pub(super) async fn queue_local_changes_with_progress(
        &self,
        local: &GaggiMateClient,
        cloud_state: &Value,
        progress: Option<(&str, bool)>,
    ) -> Result<(Vec<String>, usize), EngineError> {
        let mut skipped = Vec::new();
        let suppressed = suppressed_items(cloud_state);
        let two_way_notes = two_way_notes_active(cloud_state);

        let now = Utc::now();
        self.queue_due_profiles(local, &suppressed, now, &mut skipped)
            .await?;

        if self.store.setting("notes_reader_version")?.as_deref() != Some("4") {
            self.store.set_setting("last_full_notes_scan", "")?;
            self.store.set_setting("last_recent_notes_scan", "")?;
            self.store.set_setting("notes_reader_version", "4")?;
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
        let total_shots = index.len();
        if let Some((device_id, publish)) = progress {
            self.report_sync_progress(
                device_id,
                SyncProgress {
                    phase: SyncProgressPhase::ReadingHistory,
                    scanned_shots: 0,
                    total_shots,
                    uploaded_items: None,
                    total_items: None,
                },
                publish,
            )
            .await?;
        }
        let mut last_progress_update = Instant::now();
        for (position, entry) in index.into_iter().enumerate() {
            // IDs can be reused after history maintenance. The timestamp keeps a
            // later shot from silently replacing an older cloud copy.
            let source_key = shot_source_key(&entry);
            if !suppressed.contains(&("shot".into(), source_key.clone())) {
                let fingerprint = shot_fingerprint(&entry);
                // v2 deliberately requeues shots once after the original client
                // used non-canonical JSON hashes that the API could not accept.
                let fingerprint_key = format!("shot_fingerprint_v2:{source_key}");
                let changed =
                    self.store.setting(&fingerprint_key)?.as_deref() != Some(&fingerprint);
                let shot_readable = if changed {
                    match local.shot(entry.id).await {
                        Ok(mut shot) => {
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
                            true
                        }
                        Err(error) => {
                            let reason = shot_read_failure(&error);
                            self.store.record_failure(
                                None,
                                "shot",
                                &source_key,
                                "read",
                                &reason,
                            )?;
                            skipped.push(format!("Shot {} could not be read: {reason}", entry.id));
                            false
                        }
                    }
                } else {
                    true
                };
                if shot_readable
                    && should_refresh_notes(changed, full_notes, recent_notes, position)
                    && !suppressed.contains(&("notes".into(), source_key.clone()))
                {
                    match local.notes(entry.id).await {
                        Ok(notes) => {
                            self.store
                                .clear_failure_stage("notes", &source_key, "read")?;
                            let notes =
                                normalized_notes(notes.unwrap_or_else(|| serde_json::json!({})));
                            let empty = notes_are_semantically_empty(&notes);
                            // One-way sync keeps ignoring untouched Notes. The
                            // Protocol-2 two-way baseline needs every empty Note
                            // so the API can map Brews that appeared after setup.
                            if !empty || two_way_notes {
                                self.store.queue(&SyncObject {
                                    kind: "notes".into(),
                                    source_key: source_key.clone(),
                                    source_hash: hash_value(&notes),
                                    shot_source_key: Some(source_key.clone()),
                                    data: notes,
                                })?;
                            }
                        }
                        Err(error) => {
                            let reason = error.to_string();
                            self.store.record_failure(
                                None,
                                "notes",
                                &source_key,
                                "read",
                                &reason,
                            )?;
                            skipped.push(format!(
                                "Notes for shot {} could not be read: {reason}",
                                entry.id
                            ))
                        }
                    }
                }
            }
            let scanned_shots = position + 1;
            if let Some((device_id, publish)) = progress {
                if scanned_shots == total_shots
                    || last_progress_update.elapsed() >= StdDuration::from_secs(5)
                {
                    self.report_sync_progress(
                        device_id,
                        SyncProgress {
                            phase: SyncProgressPhase::ReadingHistory,
                            scanned_shots,
                            total_shots,
                            uploaded_items: None,
                            total_items: None,
                        },
                        publish,
                    )
                    .await?;
                    last_progress_update = Instant::now();
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
        Ok((skipped, total_shots))
    }

    pub(super) async fn queue_due_profiles(
        &self,
        local: &GaggiMateClient,
        suppressed: &HashSet<(String, String)>,
        now: DateTime<Utc>,
        skipped: &mut Vec<String>,
    ) -> Result<(), EngineError> {
        let last_profiles = parse_time(self.store.setting("last_profile_scan")?);
        if !scan_due(now, last_profiles, Duration::minutes(5)) {
            return Ok(());
        }
        for (id, loaded) in local.profiles().await? {
            if suppressed.contains(&("profile".into(), id.clone())) {
                continue;
            }
            let data = match loaded {
                Ok(data) => data,
                Err(error) => {
                    let reason = error.to_string();
                    self.store
                        .record_failure(None, "profile", &id, "read", &reason)?;
                    skipped.push(format!("Profile {id} could not be read: {reason}"));
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
        Ok(())
    }

    pub(super) async fn flush_queue_with_progress(
        &self,
        device_id: &str,
        progress: Option<(usize, usize)>,
    ) -> Result<(usize, usize), EngineError> {
        let mut invalid = 0;
        let total_items = self.store.pending_count()?;
        let mut uploaded_items = 0;
        if let Some((scanned_shots, total_shots)) = progress {
            self.report_sync_progress(
                device_id,
                SyncProgress {
                    phase: SyncProgressPhase::Uploading,
                    scanned_shots,
                    total_shots,
                    uploaded_items: Some(uploaded_items),
                    total_items: Some(total_items),
                },
                true,
            )
            .await?;
        }
        let mut last_progress_update = Instant::now();
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
                        uploaded_items += 1;
                    }
                }
            }
            if let Some((scanned_shots, total_shots)) = progress {
                if last_progress_update.elapsed() >= StdDuration::from_secs(5) {
                    self.report_sync_progress(
                        device_id,
                        SyncProgress {
                            phase: SyncProgressPhase::Uploading,
                            scanned_shots,
                            total_shots,
                            uploaded_items: Some(uploaded_items),
                            total_items: Some(total_items),
                        },
                        true,
                    )
                    .await?;
                    last_progress_update = Instant::now();
                }
            }
        }
        Ok((invalid, total_items))
    }

    pub(super) async fn write_and_verify_notes(
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

    pub(super) async fn process_outbound_notes(
        &self,
        local: &GaggiMateClient,
        device_id: &str,
    ) -> Result<(), EngineError> {
        let claim = self.cloud.claim_outbound_notes(device_id).await?;
        if claim.get("status").and_then(Value::as_str) == Some("backup_required") {
            self.store.record_failure(
                None,
                "notes",
                "outbound",
                "backup",
                "A manual Latest Backup is required before Notes can be updated.",
            )?;
            return Ok(());
        }
        self.store
            .clear_failure_stage("notes", "outbound", "backup")?;
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
            let desired = operation
                .get("desiredNotes")
                .or_else(|| operation.get("desired_data"))
                .cloned()
                .ok_or(CloudError::Rejected)?;
            let desired = normalized_notes(desired);
            let desired_hash = hash_value(&desired);
            let current = match local.notes(machine_id).await {
                Ok(notes) => normalized_notes(notes.unwrap_or_else(|| serde_json::json!({}))),
                Err(error) => {
                    let reason = error.to_string();
                    self.store
                        .record_failure(None, "notes", source_key, "write", &reason)?;
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
            // A prior write can have succeeded even if its completion request
            // was interrupted. Treat the already verified target as applied
            // instead of turning its retry into a false conflict.
            if current_hash == desired_hash {
                self.store
                    .clear_failure_stage("notes", source_key, "write")?;
                self.cloud
                    .complete_outbound_note(
                        device_id,
                        operation_id,
                        serde_json::json!({
                            "leaseToken": lease_token,
                            "status": "applied",
                            "actualHash": current_hash,
                        }),
                    )
                    .await?;
                continue;
            }
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
                    let reason = error.to_string();
                    self.store
                        .record_failure(None, "notes", source_key, "write", &reason)?;
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
            let first_sync = self.status().await.last_sync_at.is_none();
            let local = GaggiMateClient::new(&host)?;
            self.process_profile_store_operations(&local, &device_id, 0)
                .await?;
            if !configured {
                self.clear_sync_progress().await;
                self.cloud
                    .heartbeat(&device_id, true, None, None, None)
                    .await?;
                let mut status = self.status.write().await;
                status.machine_reachable = true;
                return Ok(());
            }
            // An in-place app update keeps the device ID. Advertise the new
            // protocol before its first repaired empty-Notes batch reaches
            // the API, while avoiding an extra heartbeat on every sync cycle.
            if self
                .store
                .setting(TWO_WAY_NOTES_PROTOCOL_ANNOUNCED_SETTING)?
                .as_deref()
                != Some(TWO_WAY_NOTES_PROTOCOL_VERSION)
            {
                self.cloud
                    .heartbeat(&device_id, true, None, None, None)
                    .await?;
                self.store.set_setting(
                    TWO_WAY_NOTES_PROTOCOL_ANNOUNCED_SETTING,
                    TWO_WAY_NOTES_PROTOCOL_VERSION,
                )?;
            }
            // The Store bridge and the regular synchronizer both speak the
            // GaggiMate WebSocket protocol. Keep their machine requests
            // serialized even though their cloud work is independent.
            let skipped = {
                let _machine_guard = self.profile_store_lock.lock().await;
                self.queue_local_changes_with_progress(
                    &local,
                    &state,
                    first_sync.then_some((device_id.as_str(), !cloud_unreachable)),
                )
                .await?
            };
            if cloud_unreachable {
                // Local changes are safely queued before reporting the missing
                // internet connection. They are uploaded on the next cycle.
                return Err(CloudError::Unreachable.into());
            }
            let (skipped, total_shots) = skipped;
            let (invalid, total_items) = self
                .flush_queue_with_progress(
                    &device_id,
                    first_sync.then_some((total_shots, total_shots)),
                )
                .await?;
            if first_sync {
                self.report_sync_progress(
                    &device_id,
                    SyncProgress {
                        phase: SyncProgressPhase::Finishing,
                        scanned_shots: total_shots,
                        total_shots,
                        uploaded_items: Some(
                            total_items.saturating_sub(self.store.pending_count()?),
                        ),
                        total_items: Some(total_items),
                    },
                    true,
                )
                .await?;
            }
            {
                let _machine_guard = self.profile_store_lock.lock().await;
                self.process_outbound_notes(&local, &device_id).await?;
            }
            let synchronized_at = api_timestamp(Utc::now());
            let warning_code =
                (!skipped.is_empty() || invalid > 0).then_some("LOCAL_ITEMS_SKIPPED");
            self.clear_sync_progress().await;
            self.cloud
                .heartbeat(&device_id, true, Some(&synchronized_at), warning_code, None)
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
            status.last_error_code = warning_code.map(str::to_string);
            status.last_error_at = warning_code.map(|_| api_timestamp(Utc::now()));
            status.issues = self.store.failures().unwrap_or_default();
            Ok::<(), EngineError>(())
        }
        .await;
        if let Err(error) = &result {
            if matches!(
                error,
                EngineError::Cloud(CloudError::Revoked | CloudError::ReauthRequired)
            ) {
                // Device revocation is checked by the API for every request.
                // Clear credentials and queued account data immediately so a
                // revoked installation cannot keep presenting itself as linked.
                self.credentials.delete_tokens()?;
                self.store.clear_account_data()?;
                self.store
                    .set_setting("reconnect_required_reason", error.heartbeat_code())?;
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
                    last_error: Some(error.to_string()),
                    last_error_code: Some(error.heartbeat_code().to_string()),
                    last_error_at: Some(api_timestamp(Utc::now())),
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
                drop(guard);
                return result;
            }
            let message = error.to_string();
            let error_code = error.heartbeat_code();
            let error_at = api_timestamp(Utc::now());
            let (machine_reachable, last_sync_at) = {
                let mut status = self.status.write().await;
                status.machine_reachable = error.machine_reachable();
                status.last_error = Some(message.clone());
                status.last_error_code = Some(error_code.to_string());
                status.last_error_at = Some(error_at);
                status.sync_progress = None;
                (status.machine_reachable, status.last_sync_at.clone())
            };
            let _ = self
                .cloud
                .heartbeat(
                    &device_id,
                    machine_reachable,
                    last_sync_at.as_deref(),
                    Some(error_code),
                    None,
                )
                .await;
        }
        self.status.write().await.syncing = false;
        drop(guard);
        result
    }
}
