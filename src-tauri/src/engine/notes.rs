use std::collections::HashSet;

use serde_json::Value;

use crate::{cloud::CloudError, model::AppStatus};

use super::{hash_value, normalized_notes, EngineError, SyncEngine};

impl SyncEngine {
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

    async fn refresh_notes_activation_status(&self) -> Result<AppStatus, String> {
        if !self.status().await.connected {
            return Err(
                "Connect this installation to MyBrewFolio before enabling Notes Sync.".into(),
            );
        }
        let device_id = self.device_id().map_err(|error| error.to_string())?;
        let state = self
            .cloud
            .state(&device_id)
            .await
            .map_err(|error| error.to_string())?;
        let source = &state["source"];
        let mode = source
            .get("notes_sync_status")
            .or_else(|| source.get("notesSyncStatus"));
        if !matches!(
            mode.and_then(Value::as_str),
            Some("one_way" | "activation_pending" | "two_way")
        ) {
            return Err(
                "MyBrewFolio did not return a valid Notes Sync status. Please retry.".into(),
            );
        }
        self.update_from_cloud_state(&state).await;
        Ok(self.status().await)
    }

    pub async fn prepare_headless_notes_activation(&self) -> Result<Value, String> {
        let _sync = self.sync_lock.lock().await;
        let _machine = self.profile_store_lock.lock().await;
        let status = self.refresh_notes_activation_status().await?;
        if status.notes_sync_status == "two_way" {
            return if status.notes_sync_writer_device_id == status.this_device_id {
                Ok(serde_json::json!({ "alreadyEnabled": true }))
            } else {
                Err(
                    "Two-way Notes Sync is assigned to another installation. No changes were made."
                        .into(),
                )
            };
        }
        if status.notes_sync_status == "activation_pending"
            && status.notes_sync_target_device_id != status.this_device_id
        {
            return Err(
                "Notes Sync activation is assigned to another installation. No changes were made."
                    .into(),
            );
        }
        self.begin_two_way_notes_activation()
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn activate_headless_notes(
        &self,
        backup_id: &str,
        decisions: Value,
    ) -> Result<Value, String> {
        let _sync = self.sync_lock.lock().await;
        let _machine = self.profile_store_lock.lock().await;
        let status = self.refresh_notes_activation_status().await?;
        if status.notes_sync_status != "activation_pending"
            || status.notes_sync_target_device_id != status.this_device_id
        {
            return Err("Notes Sync activation is no longer assigned to this installation. Run notes enable again.".into());
        }
        self.activate_two_way_notes(backup_id, decisions)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn begin_two_way_notes_activation(&self) -> Result<Value, EngineError> {
        let device_id = self.device_id()?;
        let status = self.status.read().await.clone();
        if status.notes_sync_status == "one_way" {
            self.cloud.request_two_way_notes(&device_id).await?;
        } else if status.notes_sync_status == "two_way"
            || status.notes_sync_target_device_id.as_deref() != Some(device_id.as_str())
        {
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
}
