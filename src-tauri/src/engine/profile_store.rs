use serde_json::{json, Value};

use crate::{cloud::CloudError, local::GaggiMateClient, store::BridgeCompletion};

use super::{
    apply_profile_actions, profile_favorite_count, profile_fetch_operation,
    profile_install_operation, profile_install_preview_operation, profile_inventory_operation,
    profile_store_issue_source_key, profile_store_public_error_message, EngineError, SyncEngine,
};

impl SyncEngine {
    pub(super) async fn execute_profile_store_operation(
        &self,
        local: &GaggiMateClient,
        operation_type: &str,
        payload: &Value,
    ) -> Result<Value, (&'static str, String)> {
        match operation_type {
            "profile_inventory" => profile_inventory_operation(local).await,
            "profile_fetch" => profile_fetch_operation(local, payload).await,
            "profile_install_preview" => profile_install_preview_operation(local, payload).await,
            "profile_install" => profile_install_operation(local, payload).await,
            _ => Err((
                "UNSUPPORTED_OPERATION",
                "This Profile Store operation is not supported".into(),
            )),
        }
    }

    pub(super) async fn flush_profile_store_completions(
        &self,
        local: &GaggiMateClient,
        device_id: &str,
    ) -> Result<(), EngineError> {
        for completion in self.store.bridge_completions(8)? {
            let Some(payload) = self
                .profile_store_completion_payload(local, &completion)
                .await?
            else {
                continue;
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

    pub(super) async fn profile_store_completion_payload(
        &self,
        local: &GaggiMateClient,
        completion: &BridgeCompletion,
    ) -> Result<Option<Value>, EngineError> {
        if !completion
            .payload
            .get("profileStoreConfirmationPending")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(Some(completion.payload.clone()));
        }
        let Some(profile_id) = completion
            .payload
            .get("profileId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            self.store
                .remove_bridge_completion(&completion.operation_id)?;
            return Ok(None);
        };
        let confirmed = match local.load_profile(profile_id).await {
            Ok(value) => value,
            // This is deliberately not terminal. The save was acknowledged already, so a slow or
            // briefly unreachable machine must remain "confirming".
            Err(_) => return Ok(None),
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
            let (action_failures, favorite_applied, selected_applied) =
                apply_profile_actions(local, profile_id, favorite, selected).await;
            let favorite_count =
                profile_favorite_count(&local.profile_inventory().await.unwrap_or_default());
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
        Ok(Some(completed))
    }

    pub(super) async fn process_profile_store_operations(
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
                Err((code, message)) => {
                    if matches!(
                        code,
                        "GAGGIMATE_UNREACHABLE"
                            | "GAGGIMATE_DATA_INVALID"
                            | "GAGGIMATE_HOST_INVALID"
                            | "GAGGIMATE_SHOT_FORMAT_UNSUPPORTED"
                            | "PROFILE_LOAD_FAILED"
                            | "PROFILE_SAVE_FAILED"
                    ) {
                        let source_key = profile_store_issue_source_key(&payload);
                        self.store.record_failure(
                            None,
                            "profile",
                            &source_key,
                            "store",
                            &message,
                        )?;
                    }
                    json!({
                        "leaseToken": lease_token,
                        "status": "failed",
                        "errorCode": code,
                        "errorMessage": profile_store_public_error_message(code, &message),
                    })
                }
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
}
