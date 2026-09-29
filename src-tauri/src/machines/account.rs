// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::collections::HashSet;

impl MachineManager {
    pub async fn after_account_connected(&self) -> Result<(), String> {
        let _account = self.account_gate.write().await;
        self.stop_all_workers().await;
        self.auth_loss_handled.store(false, Ordering::SeqCst);
        let source_id = self
            .registry
            .setting("source_id")
            .map_err(|error| error.to_string())?
            .ok_or("MyBrewFolio did not return a machine ID")?;
        let device_id = self
            .registry
            .setting("device_id")
            .map_err(|error| error.to_string())?
            .ok_or("MyBrewFolio did not return an installation connection")?;
        let previous = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?;
        if previous
            .iter()
            .any(|machine| machine.legacy_default && machine.id != source_id)
        {
            // A different default source means a different account. Never
            // attach its prior local queues to this newly authenticated user.
            for machine in previous.iter().filter(|machine| !machine.legacy_default) {
                if let Some(engine) = self.engines.read().await.get(&machine.id).cloned() {
                    engine
                        .clear_local_account_data()
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            self.registry
                .clear_machines()
                .map_err(|error| error.to_string())?;
            self.engines.write().await.clear();
            self.run_gates.write().await.clear();
        }
        let existing = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?;
        let name = existing
            .iter()
            .find(|machine| machine.id == source_id)
            .map(|machine| machine.name.clone())
            .unwrap_or_else(|| "GaggiMate".into());
        self.registry
            .save_machine(&MachineRecord {
                id: source_id.clone(),
                name,
                device_id: Some(device_id),
                active: true,
                pending_detach: false,
                legacy_default: true,
            })
            .map_err(|error| error.to_string())?;
        self.engines
            .write()
            .await
            .insert(source_id, self.primary.clone());
        if let Some(default_id) = self
            .registry
            .setting("source_id")
            .map_err(|error| error.to_string())?
        {
            self.run_gates
                .write()
                .await
                .entry(default_id)
                .or_insert_with(|| Arc::new(Mutex::new(())));
        }
        self.registry
            .remove_setting("reconnect_required_reason")
            .map_err(|error| error.to_string())?;
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        self.cloud
            .register_installation(&installation_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(super) async fn stop_all_workers(&self) {
        let mut workers = self.worker_aborts.lock().await;
        for (_, handles) in workers.drain() {
            for handle in handles {
                handle.abort();
            }
        }
    }

    fn known_engines(&self, engines: &HashMap<String, Arc<SyncEngine>>) -> Vec<Arc<SyncEngine>> {
        let mut result = vec![self.primary.clone()];
        for engine in engines.values() {
            if !result.iter().any(|known| Arc::ptr_eq(known, engine)) {
                result.push(engine.clone());
            }
        }
        result
    }

    /// Serialize the token swap with every machine sync. After an account
    /// change, clear old queues before any background task can use new tokens.
    pub async fn complete_oauth_and_authorize(
        &self,
        callback_url: &str,
        initial_name: Option<String>,
    ) -> Result<(), String> {
        let _account = self.account_gate.write().await;
        self.stop_all_workers().await;
        let engine_map = self.engines.read().await;
        let engines = self.known_engines(&engine_map);
        drop(engine_map);
        let mut pauses = Vec::with_capacity(engines.len());
        for engine in &engines {
            pauses.push(engine.pause_operations().await);
        }
        let authorized = self
            .primary
            .complete_oauth_session(callback_url)
            .await
            .map_err(|error| error.to_string());
        drop(pauses);
        authorized?;
        self.after_oauth_authorized_inner(initial_name).await
    }

    #[cfg(feature = "headless")]
    pub async fn poll_device_oauth_and_authorize(
        &self,
        initial_name: Option<String>,
    ) -> Result<bool, String> {
        let _account = self.account_gate.write().await;
        self.stop_all_workers().await;
        let engine_map = self.engines.read().await;
        let engines = self.known_engines(&engine_map);
        drop(engine_map);
        let mut pauses = Vec::with_capacity(engines.len());
        for engine in &engines {
            pauses.push(engine.pause_operations().await);
        }
        let authorized = self
            .primary
            .poll_device_oauth_session()
            .await
            .map_err(|error| error.to_string());
        drop(pauses);
        if authorized? {
            self.after_oauth_authorized_inner(initial_name).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[cfg(feature = "headless")]
    pub async fn headless_pairing_and_authorize(
        &self,
        initial_name: Option<String>,
        initialized: bool,
    ) -> Result<(Option<String>, bool), String> {
        if initialized
            && self
                .credentials
                .tokens()
                .map_err(|error| error.to_string())?
                .is_some()
        {
            return Ok((None, true));
        }
        let _account = self.account_gate.write().await;
        let has_tokens = self
            .credentials
            .tokens()
            .map_err(|error| error.to_string())?
            .is_some();
        let url = if has_tokens {
            None
        } else {
            self.stop_all_workers().await;
            let engine_map = self.engines.read().await;
            let engines = self.known_engines(&engine_map);
            drop(engine_map);
            let mut pauses = Vec::with_capacity(engines.len());
            for engine in &engines {
                pauses.push(engine.pause_operations().await);
            }
            let result = self
                .primary
                .headless_pairing()
                .await
                .map_err(|error| error.to_string());
            drop(pauses);
            result?
        };
        let connected = self
            .credentials
            .tokens()
            .map_err(|error| error.to_string())?
            .is_some();
        if connected && !initialized {
            self.after_oauth_authorized_inner(initial_name).await?;
        }
        Ok((url, connected))
    }

    /// Complete OAuth before any machine is selected. Existing account
    /// machines must be chosen explicitly on a new computer, so a different
    /// local host can never be uploaded into the legacy default source.
    pub async fn after_oauth_authorized(&self, initial_name: Option<String>) -> Result<(), String> {
        let _account = self.account_gate.write().await;
        self.stop_all_workers().await;
        self.after_oauth_authorized_inner(initial_name).await
    }

    async fn after_oauth_authorized_inner(
        &self,
        initial_name: Option<String>,
    ) -> Result<(), String> {
        self.auth_loss_handled.store(false, Ordering::SeqCst);
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        self.cloud
            .register_installation(&installation_id)
            .await
            .map_err(|error| error.to_string())?;
        let remote = self
            .cloud
            .list_machines(&installation_id)
            .await
            .map_err(|error| error.to_string())?;
        let remote_ids: HashSet<&str> = remote
            .iter()
            .filter_map(|machine| machine.get("id").and_then(Value::as_str))
            .collect();
        let previous = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?;
        if previous.iter().any(|machine| {
            // A locally reserved ID can exist after a failed create request.
            // It has never been bound to an account and cannot prove that the
            // user changed accounts during authorization.
            machine.device_id.is_some() && !remote_ids.contains(machine.id.as_str())
        }) {
            let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
            for engine in engines {
                engine
                    .clear_local_account_data()
                    .await
                    .map_err(|error| error.to_string())?;
            }
            self.registry
                .clear_machines()
                .map_err(|error| error.to_string())?;
            self.engines.write().await.clear();
            self.run_gates.write().await.clear();
        } else {
            for machine in previous.iter().filter(|machine| machine.active) {
                let device = self
                    .cloud
                    .attach_machine(&machine.id, &installation_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if device.source_id != machine.id {
                    return Err("MyBrewFolio returned a different machine ID".into());
                }
                let host = self.engine(&machine.id).await?.status().await.machine_host;
                self.finish_attach(
                    &machine.id,
                    &machine.name,
                    &host,
                    &device.id,
                    machine.legacy_default,
                )
                .await?;
            }
        }
        if remote.is_empty()
            && self
                .registry
                .machines()
                .map_err(|error| error.to_string())?
                .is_empty()
        {
            if let Some(name) = initial_name {
                let host = self.primary.status().await.machine_host;
                self.add_machine_inner(&name, &host).await?;
            }
        }
        self.registry
            .remove_setting("reconnect_required_reason")
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}
