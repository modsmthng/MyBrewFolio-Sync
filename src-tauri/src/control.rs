// SPDX-License-Identifier: GPL-3.0-or-later
//! Account-owned actions, shared by desktop and Docker. No network listener or shell execution.
use std::{collections::HashSet, time::Duration as StdDuration};

#[cfg(feature = "headless")]
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    hash_value, normalized_notes, shot_source_key, CloudError, EngineError, NotesWriteOutcome,
    SyncEngine,
};

impl SyncEngine {
    #[cfg(feature = "headless")]
    pub async fn headless_pairing(&self) -> Result<Option<String>, EngineError> {
        if self.status().await.connected
            || self.store.setting("headless_pairing_disabled")?.as_deref() == Some("1")
        {
            return Ok(None);
        }
        let expires = self
            .store
            .setting("device_auth_expires")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        if expires <= Utc::now().timestamp()
            || self.credentials.pending_device_authorization()?.is_none()
        {
            let info = self.begin_device_oauth().await?;
            return Ok(Some(info.verification_uri));
        }
        match self.poll_device_oauth().await {
            Ok(true) => {
                eprintln!(
                    "MyBrewFolio connected. Manage Sync at https://mybrewfolio.com/account/sync"
                );
                return Ok(None);
            }
            Ok(false) => {}
            Err(EngineError::Cloud(
                CloudError::DeviceAuthorizationRejected
                | CloudError::DeviceAuthorizationExchangeFailed,
            )) => {
                let info = self.restart_rejected_device_oauth().await?;
                return Ok(Some(info.verification_uri));
            }
            Err(error) => return Err(error),
        }
        Ok(self.store.setting("device_auth_url")?)
    }

    pub async fn run_control_worker(&self) {
        loop {
            if self.status().await.connected {
                if self.process_control_operations().await.is_err() {
                    tokio::time::sleep(StdDuration::from_secs(5)).await;
                }
            } else {
                tokio::time::sleep(StdDuration::from_secs(5)).await;
            }
        }
    }

    pub async fn process_control_operations(&self) -> Result<(), EngineError> {
        let device_id = self.device_id()?;
        for completion in self.store.control_completions(25)? {
            self.cloud.control_request(&device_id, &format!("operations/{}/complete", completion.operation_id),
                json!({ "leaseToken": completion.lease_token, "status": completion.payload["status"],
                    "result": completion.payload["result"] })).await?;
            self.store
                .remove_control_completion(&completion.operation_id)?;
        }
        let claimed = self
            .cloud
            .control_request(&device_id, "operations/claim", json!({"waitSeconds":25}))
            .await?;
        for operation in claimed["operations"].as_array().into_iter().flatten() {
            let id = operation["id"]
                .as_str()
                .filter(|value| Uuid::parse_str(value).is_ok())
                .ok_or(CloudError::Rejected)?;
            let token = operation["leaseToken"]
                .as_str()
                .filter(|value| Uuid::parse_str(value).is_ok())
                .ok_or(CloudError::Rejected)?;
            let _control = self.control_lock.lock().await;
            // Persist BEFORE execution. After a crash, acknowledge interruption rather than replaying a write.
            self.store.queue_control_completion(id, token, &json!({"status":"failed", "result":{"errorCode":"INTERRUPTED",
                "message":"Sync stopped during this action. Review its state and create a new preview before trying again."}}))?;
            let result = {
                let action = self.execute_control_action(
                    id,
                    token,
                    operation["type"].as_str().unwrap_or(""),
                    &operation["payload"],
                );
                tokio::pin!(action);
                let mut renewal = tokio::time::interval_at(
                    tokio::time::Instant::now() + StdDuration::from_secs(30),
                    StdDuration::from_secs(30),
                );
                loop {
                    tokio::select! {
                        result = &mut action => break result,
                        _ = renewal.tick() => {
                            if let Err(error) = self.cloud.control_request(&device_id, &format!("operations/{id}/renew"), json!({"leaseToken":token})).await {
                                break Err(error.into());
                            }
                        }
                    }
                }
            }; // Drop the action and its machine locks before acknowledging cancellation.
            let completion = match result {
                Ok(result) => json!({"status":"completed", "result":result}),
                Err(error) => {
                    json!({"status":"failed", "result":{"errorCode":error.heartbeat_code(),
                    "message":"Action could not finish. Check the installation and review a fresh preview before retrying."}})
                }
            };
            self.store
                .queue_control_completion(id, token, &completion)?;
            self.cloud.control_request(&device_id, &format!("operations/{id}/complete"),
                json!({"leaseToken":token,"status":completion["status"],"result":completion["result"]})).await?;
            self.store.remove_control_completion(id)?;
        }
        Ok(())
    }

    async fn execute_control_action(
        &self,
        id: &str,
        token: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<Value, EngineError> {
        if kind == "sync_now" || kind == "retry" {
            {
                let _lock = self.sync_lock.lock().await;
                if kind == "retry" {
                    self.retry_failures().await?;
                }
            }
            self.sync_once().await?;
            return Ok(json!({"ok":true}));
        }
        if kind == "diagnose" {
            let status = self.status().await;
            return Ok(
                json!({"connected": status.connected, "machineReachable": status.machine_reachable,
                "lastSyncAt":status.last_sync_at,"profiles":status.profiles,"shots":status.shots,"notes":status.notes,
                "pending":self.store.pending_count()?,"failures":self.store.failure_count()?,
                "conflicts":status.conflicts,"suppressed":status.suppressed}),
            );
        }
        if kind == "notes_prepare" {
            let preview = self
                .prepare_headless_notes_activation()
                .await
                .map_err(|_| CloudError::Rejected)?;
            return Ok(
                json!({"backupId":preview["backupId"],"alreadyEnabled":preview["alreadyEnabled"]}),
            );
        }
        let _sync = self.sync_lock.lock().await;
        let _machine = self.profile_store_lock.lock().await;
        let device_id = self.device_id()?;
        let state = self.cloud.state(&device_id).await?;
        self.update_from_cloud_state(&state).await;
        match kind {
            "notes_backup" => Ok(json!({"backupId":self.create_latest_notes_backup().await?})),
            "notes_restore_preview" => {
                let backup_id = payload["backupId"].as_str().ok_or(CloudError::Rejected)?;
                let local = self.local_client()?;
                let mut preview = self.preview_notes_restore(backup_id).await?;
                for item in preview["items"].as_array_mut().into_iter().flatten() {
                    let available = item["available"].as_bool().unwrap_or(false);
                    let key = item["source_key"].as_str().unwrap_or("").to_string();
                    let current_hash = if available {
                        let shot_id = key
                            .split(':')
                            .next()
                            .and_then(|value| value.parse().ok())
                            .ok_or(CloudError::Rejected)?;
                        Some(hash_value(
                            &local.notes(shot_id).await?.unwrap_or_else(|| json!({})),
                        ))
                    } else {
                        None
                    };
                    *item = json!({"source_key":key,"shot_timestamp":item["shot_timestamp"],
                        "available":available,"currentHash":current_hash,"notes_hash":item["notes_hash"]});
                }
                Ok(preview)
            }
            "notes_restore" => self.restore_control_notes(id, token, payload).await,
            "resync_preview" => self.resync_preview().await,
            "resync_apply" => {
                // Refresh machine inventory before accepting an older user-reviewed selection.
                let current = self.resync_preview().await?;
                let ids = current["restoreItems"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if payload["restoreItemIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|id| !ids.iter().any(|item| item["id"] == *id))
                {
                    return Err(CloudError::Rejected.into());
                }
                let duplicates = current["duplicates"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if payload["duplicateResolutions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|choice| {
                        !duplicates.iter().any(|item| {
                            item["mapping_id"] == choice["mappingId"]
                                && item["keep_shot_id"] == choice["keepShotId"]
                                && item["remove_shot_id"] == choice["removeShotId"]
                        })
                    })
                {
                    return Err(CloudError::Rejected.into());
                }
                self.apply_resync(payload.clone()).await
            }
            _ => Err(CloudError::Rejected.into()),
        }
    }

    async fn restore_control_notes(
        &self,
        id: &str,
        token: &str,
        payload: &Value,
    ) -> Result<Value, EngineError> {
        let device_id = self.device_id()?;
        let snapshot = self
            .cloud
            .control_request(
                &device_id,
                &format!("operations/{id}/restore-items"),
                json!({"leaseToken":token}),
            )
            .await?;
        let items = snapshot["items"].as_array().ok_or(CloudError::Rejected)?;
        if items.is_empty() {
            return Err(CloudError::Rejected.into());
        }
        let local = self.local_client()?;
        let index: HashSet<String> = local
            .shot_index()
            .await?
            .iter()
            .map(shot_source_key)
            .collect();
        // Reject stale previews before creating the safety backup or writing any selected Note.
        for item in items {
            let key = item["source_key"].as_str().ok_or(CloudError::Rejected)?;
            if !index.contains(key) {
                return Err(CloudError::Rejected.into());
            }
            let shot_id = key
                .split(':')
                .next()
                .and_then(|value| value.parse().ok())
                .ok_or(CloudError::Rejected)?;
            let hash = hash_value(&local.notes(shot_id).await?.unwrap_or_else(|| json!({})));
            if payload["expectedHashes"][key].as_str() != Some(&hash) {
                return Err(CloudError::Rejected.into());
            }
        }
        self.create_latest_notes_backup().await?;
        let mut applied = 0;
        let mut skipped = 0;
        for item in items {
            // Revocation / disabling Notes takes effect before the next write, even during a long restore.
            self.cloud
                .control_request(
                    &device_id,
                    &format!("operations/{id}/renew"),
                    json!({"leaseToken":token}),
                )
                .await?;
            let key = item["source_key"].as_str().ok_or(CloudError::Rejected)?;
            let shot_id = key
                .split(':')
                .next()
                .and_then(|value| value.parse().ok())
                .ok_or(CloudError::Rejected)?;
            let before_notes = local.notes(shot_id).await?.unwrap_or_else(|| json!({}));
            let before = hash_value(&before_notes);
            if payload["expectedHashes"][key].as_str() != Some(&before) {
                skipped += 1;
                continue;
            }
            // Reuse the normal Notes writer's bounded verification and concurrent-edit
            // handling. Empty/default-shaped firmware Notes have the same meaning.
            match self
                .write_and_verify_notes(
                    &local,
                    shot_id,
                    &hash_value(&normalized_notes(before_notes)),
                    &item["notes_data"],
                )
                .await?
            {
                NotesWriteOutcome::Applied(_) => {}
                NotesWriteOutcome::Conflict(_) | NotesWriteOutcome::Unverified => {
                    skipped += 1;
                    continue;
                }
            }
            // Confirm the snapshot hash, including legacy backups whose empty Notes
            // have a default-shaped object. The API normalizes the restored mapping.
            self.cloud.control_request(&device_id, &format!("operations/{id}/restore-results"),
                json!({"leaseToken":token,"items":[{"sourceKey":key,"verifiedHash":item["notes_hash"]}]})).await?;
            // A selected machine Note may have no cloud mapping. It was still restored.
            applied += 1;
        }
        Ok(json!({"applied":applied,"skipped":skipped}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::{
        cloud_server, configure_test_cloud, connect_test_engine, test_engine,
    };

    #[tokio::test]
    async fn web_diagnostics_do_not_disclose_local_addresses_or_credentials() {
        let (engine, _directory) = test_engine();
        engine.set_host("192.168.1.77").await.unwrap();
        let result = engine
            .execute_control_action("unused", "unused", "diagnose", &json!({}))
            .await
            .unwrap();
        assert!(!result.to_string().contains("192.168.1.77"));
        assert!(result.get("machineHost").is_none());
        assert!(result.get("guidance").is_none());
        assert!(result.get("pending").is_some());
    }

    #[tokio::test]
    async fn completed_actions_flush_and_interrupted_actions_are_not_executed_again() {
        let (mut engine, _directory) = test_engine();
        connect_test_engine(&engine);
        let api = cloud_server(vec![r#"{"operations":[{"id":"00000000-0000-4000-8000-000000000001","leaseToken":"00000000-0000-4000-8000-000000000002","type":"diagnose","payload":{}}]}"#, "{}", "{}", r#"{"operations":[]}"#]).await;
        configure_test_cloud(&mut engine, &api);
        engine.process_control_operations().await.unwrap();
        assert!(engine.store.control_completions(25).unwrap().is_empty());
        engine
            .store
            .queue_control_completion(
                "00000000-0000-4000-8000-000000000003",
                "00000000-0000-4000-8000-000000000004",
                &json!({"status":"failed","result":{"errorCode":"INTERRUPTED"}}),
            )
            .unwrap();
        engine.process_control_operations().await.unwrap();
        assert!(engine.store.control_completions(25).unwrap().is_empty());
    }

    #[tokio::test]
    async fn scan_reset_preserves_control_acknowledgements_and_disconnect_removes_them() {
        let (engine, _directory) = test_engine();
        engine
            .store
            .queue_control_completion("operation", "lease", &json!({"status":"completed"}))
            .unwrap();
        engine.store.reset_scan_state().unwrap();
        assert_eq!(engine.store.control_completions(25).unwrap().len(), 1);
        engine.store.clear_account_data().unwrap();
        assert!(engine.store.control_completions(25).unwrap().is_empty());
    }

    #[tokio::test]
    async fn restart_waits_for_remote_work_and_keeps_machine_operations_paused() {
        let (engine, _directory) = test_engine();
        let action = engine.control_lock.lock().await;
        assert!(engine.status().await.syncing);
        assert!(
            tokio::time::timeout(StdDuration::from_millis(10), engine.pause_operations())
                .await
                .is_err()
        );
        drop(action);
        let pause = engine.pause_operations().await;
        assert!(engine.status().await.syncing);
        assert!(engine.sync_lock.try_lock().is_err());
        assert!(engine.profile_store_lock.try_lock().is_err());
        drop(pause);
        engine.status.write().await.syncing = true;
        assert!(!engine.status().await.syncing);
    }

    #[cfg(feature = "headless")]
    #[tokio::test]
    async fn automatic_pairing_reuses_pending_request_and_respects_local_disconnect() {
        let (mut engine, _directory) = test_engine();
        let api = cloud_server(vec![r#"{"requestId":"request-1","userCode":"ABCD-1234","verificationUri":"https://example.test/pair","pollToken":"poll-token","expiresIn":600}"#]).await;
        configure_test_cloud(&mut engine, &api);
        assert_eq!(
            engine.headless_pairing().await.unwrap().as_deref(),
            Some("https://example.test/pair")
        );
        assert!(engine
            .store
            .setting("device_auth_expires")
            .unwrap()
            .is_some());
        engine.disconnect().await.unwrap();
        assert!(engine.headless_pairing().await.unwrap().is_none());
    }
}
