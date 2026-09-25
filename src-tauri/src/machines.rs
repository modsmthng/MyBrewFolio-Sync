// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::{
    sync::{Mutex, RwLock},
    task::{AbortHandle, JoinHandle},
};
use uuid::Uuid;

use crate::{
    cloud::{CloudClient, CloudError},
    credentials::CredentialStore,
    engine::{EngineError, SyncEngine},
    local::normalize_host,
    store::{AppStore, MachineRecord, StoreError},
};

/// One account session, with a private queue and scan state for each machine.
/// The original sync.sqlite remains the default machine's database.
pub struct MachineManager {
    root: PathBuf,
    registry: Arc<AppStore>,
    primary: Arc<SyncEngine>,
    cloud: Arc<CloudClient>,
    credentials: Arc<dyn CredentialStore>,
    engines: RwLock<HashMap<String, Arc<SyncEngine>>>,
    run_gates: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    /// Serializes changes to server attachments and their local pending-detach state.
    attachment_gate: Mutex<()>,
    account_gate: RwLock<()>,
    next_machine: AtomicUsize,
    auth_loss_handled: AtomicBool,
    worker_aborts: Mutex<HashMap<String, Vec<AbortHandle>>>,
}

impl MachineManager {
    pub fn open(
        root: &Path,
        registry: Arc<AppStore>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, EngineError> {
        let cloud = Arc::new(CloudClient::new(credentials.clone())?);
        Self::open_with_cloud(root, registry, credentials, cloud)
    }

    fn open_with_cloud(
        root: &Path,
        registry: Arc<AppStore>,
        credentials: Arc<dyn CredentialStore>,
        cloud: Arc<CloudClient>,
    ) -> Result<Self, EngineError> {
        let primary = Arc::new(SyncEngine::open_with_cloud(
            registry.clone(),
            credentials.clone(),
            cloud.clone(),
        )?);
        let mut engines = HashMap::new();
        let mut run_gates = HashMap::new();
        for machine in registry.machines()? {
            let engine = if machine.legacy_default {
                primary.clone()
            } else {
                Self::open_extra_engine(root, &machine, &registry, &credentials, &cloud)?
            };
            run_gates.insert(machine.id.clone(), Arc::new(Mutex::new(())));
            engines.insert(machine.id, engine);
        }
        Ok(Self {
            root: root.to_path_buf(),
            registry,
            primary,
            cloud,
            credentials,
            engines: RwLock::new(engines),
            run_gates: RwLock::new(run_gates),
            attachment_gate: Mutex::new(()),
            account_gate: RwLock::new(()),
            next_machine: AtomicUsize::new(0),
            auth_loss_handled: AtomicBool::new(false),
            worker_aborts: Mutex::new(HashMap::new()),
        })
    }

    fn open_extra_engine(
        root: &Path,
        machine: &MachineRecord,
        registry: &AppStore,
        credentials: &Arc<dyn CredentialStore>,
        cloud: &Arc<CloudClient>,
    ) -> Result<Arc<SyncEngine>, EngineError> {
        // UUID syntax is enforced before constructing a path from an API ID.
        let id = Uuid::parse_str(&machine.id).map_err(|_| StoreError::InvalidCredentials)?;
        let path = root.join(format!("machine-{id}.sqlite"));
        let store = Arc::new(AppStore::open(&path)?);
        if let Some(installation_id) = registry.setting("installation_id")? {
            store.set_setting("installation_id", &installation_id)?;
        }
        if let Some(device_id) = &machine.device_id {
            store.set_setting("device_id", device_id)?;
            store.set_setting("source_id", &machine.id)?;
        }
        Ok(Arc::new(SyncEngine::open_with_cloud(
            store,
            credentials.clone(),
            cloud.clone(),
        )?))
    }

    pub fn primary(&self) -> Arc<SyncEngine> {
        self.primary.clone()
    }

    pub fn registry(&self) -> &Arc<AppStore> {
        &self.registry
    }

    /// Hold this while a UI or CLI command uses a machine engine directly.
    /// Release it before calling another manager operation or emitting status.
    pub async fn account_operation(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.account_gate.read().await
    }

    fn installation_id(&self) -> Result<String, StoreError> {
        if let Some(id) = self.registry.setting("installation_id")? {
            return Ok(id);
        }
        let id = Uuid::new_v4().to_string();
        self.registry.set_setting("installation_id", &id)?;
        Ok(id)
    }

    pub fn validate_name(name: &str) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 24 {
            return Err("Machine name must contain 1–24 characters".into());
        }
        Ok(name.to_owned())
    }

    fn local_name_available(&self, name: &str, except_id: Option<&str>) -> Result<(), String> {
        let folded = name.to_lowercase();
        if self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .iter()
            .any(|machine| {
                machine.device_id.is_some()
                    && Some(machine.id.as_str()) != except_id
                    && machine.name.to_lowercase() == folded
            })
        {
            return Err("A machine with this name already exists".into());
        }
        Ok(())
    }

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

    async fn stop_all_workers(&self) {
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

    pub async fn list_account_machines(&self) -> Result<Vec<Value>, String> {
        let _account = self.account_gate.read().await;
        let _attachment = self.attachment_gate.lock().await;
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        let local = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?;
        let mut remote = self
            .cloud
            .list_machines(&installation_id)
            .await
            .map_err(|error| error.to_string())?;
        for machine in &mut remote {
            if let Some(id) = machine.get("id").and_then(Value::as_str).map(str::to_owned) {
                if let (Some(mut saved), Some(name)) = (
                    local.iter().find(|item| item.id == id).cloned(),
                    machine.get("name").and_then(Value::as_str),
                ) {
                    if saved.name != name {
                        saved.name = name.to_owned();
                        self.registry
                            .save_machine(&saved)
                            .map_err(|error| error.to_string())?;
                    }
                }
                if let Some(object) = machine.as_object_mut() {
                    object.insert("machineId".into(), Value::String(id.clone()));
                    object.insert(
                        "connected".into(),
                        Value::Bool(local.iter().any(|item| item.id == id && item.active)),
                    );
                }
            }
        }
        Ok(remote)
    }

    pub async fn add_machine(&self, name: &str, host: &str) -> Result<String, String> {
        let _account = self.account_gate.read().await;
        self.add_machine_inner(name, host).await
    }

    async fn add_machine_inner(&self, name: &str, host: &str) -> Result<String, String> {
        let _attachment = self.attachment_gate.lock().await;
        let name = Self::validate_name(name)?;
        let host = normalize_host(host).map_err(|error| error.to_string())?;
        let retry = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|machine| {
                machine.name.to_lowercase() == name.to_lowercase() && machine.device_id.is_none()
            });
        if retry.is_none() {
            self.local_name_available(&name, None)?;
        }
        let id = retry
            .as_ref()
            .map(|machine| machine.id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.registry
            .save_machine(&MachineRecord {
                id: id.clone(),
                name: name.clone(),
                device_id: None,
                active: false,
                pending_detach: false,
                legacy_default: false,
            })
            .map_err(|error| error.to_string())?;
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        let (device, legacy_default) = self
            .cloud
            .create_machine(&id, &name, &installation_id)
            .await
            .map_err(|error| error.to_string())?;
        if device.source_id != id {
            return Err("MyBrewFolio returned a different machine ID".into());
        }
        self.finish_attach(&id, &name, &host, &device.id, legacy_default)
            .await?;
        Ok(id)
    }

    pub async fn connect_machine(&self, machine_id: &str, host: &str) -> Result<(), String> {
        let _account = self.account_gate.read().await;
        let _attachment = self.attachment_gate.lock().await;
        Uuid::parse_str(machine_id).map_err(|_| "Invalid machine ID")?;
        let host = normalize_host(host).map_err(|error| error.to_string())?;
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        let remote = self
            .cloud
            .list_machines(&installation_id)
            .await
            .map_err(|error| error.to_string())?;
        let selected = remote
            .iter()
            .find(|machine| machine.get("id").and_then(Value::as_str) == Some(machine_id))
            .ok_or("Machine is not available in this account")?;
        let name = selected
            .get("name")
            .and_then(Value::as_str)
            .ok_or("Machine is not available in this account")?;
        let device = self
            .cloud
            .attach_machine(machine_id, &installation_id)
            .await
            .map_err(|error| error.to_string())?;
        if device.source_id != machine_id {
            return Err("MyBrewFolio returned a different machine ID".into());
        }
        let legacy = selected.get("legacyDefault").and_then(Value::as_bool) == Some(true);
        self.finish_attach(machine_id, name, &host, &device.id, legacy)
            .await
    }

    async fn finish_attach(
        &self,
        machine_id: &str,
        name: &str,
        host: &str,
        device_id: &str,
        legacy_default: bool,
    ) -> Result<(), String> {
        let engine = if legacy_default {
            self.primary.clone()
        } else if let Some(engine) = self.engines.read().await.get(machine_id).cloned() {
            engine
        } else {
            let record = MachineRecord {
                id: machine_id.to_owned(),
                name: name.to_owned(),
                device_id: Some(device_id.to_owned()),
                active: true,
                pending_detach: false,
                legacy_default: false,
            };
            Self::open_extra_engine(
                &self.root,
                &record,
                &self.registry,
                &self.credentials,
                &self.cloud,
            )
            .map_err(|error| error.to_string())?
        };
        engine
            .set_machine_attachment(machine_id, device_id)
            .await
            .map_err(|error| error.to_string())?;
        engine
            .set_host(host)
            .await
            .map_err(|error| error.to_string())?;
        self.registry
            .save_machine(&MachineRecord {
                id: machine_id.to_owned(),
                name: name.to_owned(),
                device_id: Some(device_id.to_owned()),
                active: true,
                pending_detach: false,
                legacy_default,
            })
            .map_err(|error| error.to_string())?;
        self.engines
            .write()
            .await
            .insert(machine_id.to_owned(), engine);
        self.run_gates
            .write()
            .await
            .entry(machine_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())));
        Ok(())
    }

    pub async fn rename_machine(&self, machine_id: &str, name: &str) -> Result<(), String> {
        let _account = self.account_gate.read().await;
        let _attachment = self.attachment_gate.lock().await;
        let name = Self::validate_name(name)?;
        self.local_name_available(&name, Some(machine_id))?;
        let mut machine = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|machine| machine.id == machine_id)
            .ok_or("Machine is not connected to this installation")?;
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        self.cloud
            .rename_machine(machine_id, &installation_id, &name)
            .await
            .map_err(|error| error.to_string())?;
        machine.name = name;
        self.registry
            .save_machine(&machine)
            .map_err(|error| error.to_string())
    }

    pub async fn remove_machine(self: &Arc<Self>, machine_id: &str) -> Result<(), String> {
        let account = self.account_gate.read().await;
        let attachment = self.attachment_gate.lock().await;
        let mut machine = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|machine| machine.id == machine_id && machine.active)
            .ok_or("Machine is not connected to this installation")?;
        let engine = self.engine(machine_id).await?;
        let gate = self
            .run_gates
            .read()
            .await
            .get(machine_id)
            .cloned()
            .ok_or("Machine is not available locally")?;
        let _run = gate.lock().await;
        machine.active = false;
        machine.pending_detach = true;
        self.registry
            .save_machine(&machine)
            .map_err(|error| error.to_string())?;
        self.stop_workers(machine_id).await;
        let _guards = engine.pause_operations().await;
        drop(_guards);
        drop(_run);
        drop(attachment);
        drop(account);
        // Local removal takes effect immediately. Keep the server request alive
        // after returning so a cancelled timeout cannot detach a later reconnect.
        // The periodic cycle retries failures while the pending flag remains set.
        let manager = self.clone();
        tokio::spawn(async move {
            let _ = manager.flush_pending_detaches().await;
        });
        Ok(())
    }

    pub async fn flush_pending_detaches(&self) -> Result<(), String> {
        let _account = self.account_gate.read().await;
        let _attachment = self.attachment_gate.lock().await;
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        for mut machine in self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|machine| machine.pending_detach)
        {
            match self
                .cloud
                .detach_machine(&machine.id, &installation_id)
                .await
            {
                Ok(()) | Err(CloudError::Revoked) => {
                    machine.pending_detach = false;
                    machine.device_id = None;
                    self.registry
                        .save_machine(&machine)
                        .map_err(|error| error.to_string())?;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(())
    }

    pub async fn engine(&self, machine_id: &str) -> Result<Arc<SyncEngine>, String> {
        let active = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .iter()
            .any(|machine| machine.id == machine_id && machine.active);
        if !active {
            return Err("Machine is not connected to this installation".into());
        }
        self.engines
            .read()
            .await
            .get(machine_id)
            .cloned()
            .ok_or("Machine is not available locally".into())
    }

    pub async fn active_engines(&self) -> Vec<(String, Arc<SyncEngine>)> {
        let mut result = Vec::new();
        if let Ok(records) = self.registry.machines() {
            let engines = self.engines.read().await;
            for machine in records.into_iter().filter(|machine| machine.active) {
                if let Some(engine) = engines.get(&machine.id) {
                    result.push((machine.id, engine.clone()));
                }
            }
        }
        result
    }

    pub async fn register_worker<T>(&self, machine_id: &str, handle: &JoinHandle<T>) -> bool {
        let _account = self.account_gate.read().await;
        let mut workers = self.worker_aborts.lock().await;
        let active = self
            .registry
            .machines()
            .map(|records| {
                records
                    .iter()
                    .any(|machine| machine.id == machine_id && machine.active)
            })
            .unwrap_or(false);
        if !active {
            handle.abort();
            return false;
        }
        workers
            .entry(machine_id.to_owned())
            .or_default()
            .push(handle.abort_handle());
        true
    }

    pub async fn stop_workers(&self, machine_id: &str) {
        if let Some(handles) = self.worker_aborts.lock().await.remove(machine_id) {
            for handle in handles {
                handle.abort();
            }
        }
    }

    pub async fn selected_engine(
        &self,
        machine_id: Option<&str>,
    ) -> Result<Arc<SyncEngine>, String> {
        if let Some(machine_id) = machine_id {
            return self.engine(machine_id).await;
        }
        let active: Vec<_> = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|machine| machine.active)
            .collect();
        if active.len() > 1 {
            return Err("Select a machine first".into());
        }
        if let Some(machine) = active.first() {
            return self.engine(&machine.id).await;
        }
        if self
            .credentials
            .tokens()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("Connect a machine to this installation first".into());
        }
        // Before account authorization, Setup can still save the first LAN
        // address in the original store for a brand-new account.
        Ok(self.primary.clone())
    }

    pub async fn status_json(&self) -> Value {
        let mut status =
            serde_json::to_value(self.primary.status().await).unwrap_or_else(|_| json!({}));
        let mut machines = Vec::new();
        let tokens_present = self.credentials.tokens().ok().flatten().is_some();
        let mut primary_active = false;
        if let Ok(records) = self.registry.machines() {
            for machine in records.into_iter().filter(|machine| machine.active) {
                primary_active |= machine.legacy_default;
                if let Some(engine) = self.engines.read().await.get(&machine.id).cloned() {
                    let mut value =
                        serde_json::to_value(engine.status().await).unwrap_or_else(|_| json!({}));
                    if let Some(object) = value.as_object_mut() {
                        object.insert("machineId".into(), Value::String(machine.id));
                        object.insert("name".into(), Value::String(machine.name));
                        if !tokens_present {
                            object.insert("connected".into(), Value::Bool(false));
                        }
                    }
                    machines.push(value);
                }
            }
        }
        if let Some(object) = status.as_object_mut() {
            object.insert("machines".into(), Value::Array(machines));
            if tokens_present {
                object.insert("connected".into(), Value::Bool(true));
                if !primary_active {
                    object.insert("lastError".into(), Value::Null);
                    object.insert("lastErrorCode".into(), Value::Null);
                }
            } else {
                object.insert("connected".into(), Value::Bool(false));
                if let Ok(Some(code)) = self.registry.setting("reconnect_required_reason") {
                    object.insert("lastErrorCode".into(), Value::String(code.clone()));
                    object.insert(
                        "lastError".into(),
                        Value::String(if code == "SYNC_DEVICE_REVOKED" {
                            "This Sync installation is no longer authorized".into()
                        } else {
                            "Your MyBrewFolio connection needs to be renewed".into()
                        }),
                    );
                }
            }
        }
        status
    }

    pub async fn reconcile_auth_loss(&self) -> Result<(), String> {
        let _account = self.account_gate.write().await;
        if self
            .credentials
            .tokens()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }
        let mut reason = None;
        for (_, engine) in self.active_engines().await {
            let code = engine.status().await.last_error_code;
            if matches!(
                code.as_deref(),
                Some("SYNC_REAUTH_REQUIRED" | "SYNC_DEVICE_REVOKED")
            ) {
                reason = code;
                break;
            }
        }
        let Some(reason) = reason else {
            return Ok(());
        };
        if self.auth_loss_handled.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.stop_all_workers().await;
        for engine in self.engines.read().await.values() {
            engine
                .clear_local_account_data()
                .await
                .map_err(|error| error.to_string())?;
        }
        self.registry
            .set_setting("reconnect_required_reason", &reason)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub async fn sync_machine(&self, machine_id: &str) -> Result<(), String> {
        let _account = self.account_gate.read().await;
        let gate = self
            .run_gates
            .read()
            .await
            .get(machine_id)
            .cloned()
            .ok_or("Machine is not available locally")?;
        let _run = gate.lock().await;
        let engine = self.engine(machine_id).await?;
        engine.sync_once().await.map_err(|error| error.to_string())
    }

    pub async fn sync_selected(&self, machine_id: Option<&str>) -> Result<(), String> {
        if let Some(machine_id) = machine_id {
            return self.sync_machine(machine_id).await;
        }
        let active: Vec<_> = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|machine| machine.active)
            .collect();
        match active.as_slice() {
            [machine] => self.sync_machine(&machine.id).await,
            [] => Err("Connect a machine first".into()),
            _ => Err("Select a machine first".into()),
        }
    }

    pub async fn sync_all(&self) -> Vec<(String, Result<(), String>)> {
        let mut records: Vec<_> = self
            .registry
            .machines()
            .unwrap_or_default()
            .into_iter()
            .filter(|machine| machine.active)
            .collect();
        if records.is_empty() {
            return Vec::new();
        }
        let offset = self.next_machine.fetch_add(1, Ordering::Relaxed) % records.len();
        records.rotate_left(offset);
        join_all(records.iter().map(|machine| async move {
            (machine.id.clone(), self.sync_machine(&machine.id).await)
        }))
        .await
    }

    pub async fn disconnect_all(&self) -> Result<Value, String> {
        let _account = self.account_gate.write().await;
        self.stop_all_workers().await;
        let records = self
            .registry
            .machines()
            .map_err(|error| error.to_string())?;
        let installation_id = self.installation_id().map_err(|error| error.to_string())?;
        let mut all_server_revoked = true;
        for machine in records.iter().filter(|machine| !machine.legacy_default) {
            if let Some(engine) = self.engines.read().await.get(&machine.id).cloned() {
                engine
                    .clear_local_account_data()
                    .await
                    .map_err(|error| error.to_string())?;
            }
            if machine.device_id.is_some()
                && self
                    .cloud
                    .detach_machine(&machine.id, &installation_id)
                    .await
                    .is_err()
            {
                all_server_revoked = false;
            }
        }
        let _primary_paused = self.primary.pause_operations().await;
        let mut result = self
            .primary
            .disconnect()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(object) = result.as_object_mut() {
            let previous = object
                .get("serverRevoked")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            object.insert(
                "serverRevoked".into(),
                Value::Bool(previous && all_server_revoked),
            );
        }
        self.registry
            .clear_machines()
            .map_err(|error| error.to_string())?;
        self.engines.write().await.clear();
        self.run_gates.write().await.clear();
        self.auth_loss_handled.store(false, Ordering::SeqCst);
        Ok(result)
    }
}

#[cfg(test)]
#[path = "machines_tests.rs"]
mod tests;
