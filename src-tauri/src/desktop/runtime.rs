// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    autostart::{autostart_status, update_autostart_tray_item},
    emit_status,
    updates::run_update_check,
    StartupDiagnostics, UpdateRestartState,
};
use crate::{engine::SyncEngine, machines::MachineManager, store::AppStore};
use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tauri::Manager;
use tokio::task::JoinHandle;

struct MachineWorkers {
    control: JoinHandle<()>,
    bridge: JoinHandle<()>,
    scheduled: JoinHandle<()>,
}

impl MachineWorkers {
    fn is_running(&self) -> bool {
        !self.control.is_finished() && !self.bridge.is_finished() && !self.scheduled.is_finished()
    }

    fn abort(&self) {
        self.control.abort();
        self.bridge.abort();
        self.scheduled.abort();
    }
}

fn retain_active_workers(
    workers: &mut HashMap<String, MachineWorkers>,
    active_ids: &HashSet<String>,
) {
    workers.retain(|id, worker| {
        if active_ids.contains(id) && worker.is_running() {
            true
        } else {
            worker.abort();
            false
        }
    });
}

async fn run_profile_store_worker(app: tauri::AppHandle, engine: Arc<SyncEngine>) {
    loop {
        if engine.status().await.connected {
            if engine.wait_for_profile_store_operations().await.is_ok() {
                emit_status(&app, &engine).await;
            } else {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        } else {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn run_scheduled_worker(
    app: tauri::AppHandle,
    manager: Arc<MachineManager>,
    id: String,
    engine: Arc<SyncEngine>,
) {
    loop {
        if engine.status().await.connected {
            let _ = manager.sync_machine(&id).await;
            emit_status(&app, &engine).await;
        }
        engine.wait_for_sync_interval().await;
    }
}

async fn spawn_machine_workers(
    app: &tauri::AppHandle,
    manager: &Arc<MachineManager>,
    id: &str,
    engine: &Arc<SyncEngine>,
) -> MachineWorkers {
    let control_engine = engine.clone();
    let (control_start, control_ready) = tokio::sync::oneshot::channel();
    let control = tokio::spawn(async move {
        if control_ready.await.is_ok() {
            control_engine.run_control_worker().await;
        }
    });

    let bridge_engine = engine.clone();
    let bridge_handle = app.clone();
    let (bridge_start, bridge_ready) = tokio::sync::oneshot::channel();
    let bridge = tokio::spawn(async move {
        if bridge_ready.await.is_ok() {
            run_profile_store_worker(bridge_handle, bridge_engine).await;
        }
    });

    let scheduled_manager = manager.clone();
    let scheduled_engine = engine.clone();
    let scheduled_handle = app.clone();
    let scheduled_id = id.to_owned();
    let (scheduled_start, scheduled_ready) = tokio::sync::oneshot::channel();
    let scheduled = tokio::spawn(async move {
        if scheduled_ready.await.is_ok() {
            run_scheduled_worker(
                scheduled_handle,
                scheduled_manager,
                scheduled_id,
                scheduled_engine,
            )
            .await;
        }
    });

    if manager.register_worker(id, &control).await {
        let _ = control_start.send(());
    }
    if manager.register_worker(id, &bridge).await {
        let _ = bridge_start.send(());
    }
    if manager.register_worker(id, &scheduled).await {
        let _ = scheduled_start.send(());
    }
    MachineWorkers {
        control,
        bridge,
        scheduled,
    }
}

async fn supervise_machine_workers(app: tauri::AppHandle, manager: Arc<MachineManager>) {
    tokio::time::sleep(Duration::from_secs(8)).await;
    let mut workers = HashMap::new();
    loop {
        let active = manager.active_engines().await;
        let active_ids = active.iter().map(|(id, _)| id.clone()).collect();
        retain_active_workers(&mut workers, &active_ids);
        for (id, engine) in active {
            if workers.contains_key(&id) {
                continue;
            }
            let worker = spawn_machine_workers(&app, &manager, &id, &engine).await;
            workers.insert(id, worker);
        }
        let _ = manager.flush_pending_detaches().await;
        let _ = manager.list_account_machines().await;
        emit_status(&app, &manager.primary()).await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

pub(super) fn start_background_services(
    app: &tauri::App,
    manager: Arc<MachineManager>,
    store: Arc<AppStore>,
    startup_diagnostics: Arc<StartupDiagnostics>,
) {
    let initial_handle = app.handle().clone();
    let initial_engine = manager.primary();
    tauri::async_runtime::spawn(async move {
        emit_status(&initial_handle, &initial_engine).await;
    });

    let autostart_handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        if let Ok(status) = autostart_status(&autostart_handle).await {
            update_autostart_tray_item(&autostart_handle, &status);
        }
    });

    let background_handle = app.handle().clone();
    tauri::async_runtime::spawn(supervise_machine_workers(background_handle, manager));

    let update_handle = app.handle().clone();
    let update_restart_state = app.state::<Arc<UpdateRestartState>>().inner().clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let _ = run_update_check(&update_handle, &store, &update_restart_state, false).await;
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        }
    });

    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if !startup_diagnostics.frontend_ready.load(Ordering::SeqCst) {
            startup_diagnostics.append(&format!(
                "frontend_timeout=true\nfrontend_timeout_utc={}",
                chrono::Utc::now().to_rfc3339()
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{retain_active_workers, MachineWorkers};
    use std::{
        collections::{HashMap, HashSet},
        time::Duration,
    };

    #[tokio::test]
    async fn removed_machine_workers_are_aborted_without_stopping_other_machines() {
        let make_workers = || MachineWorkers {
            control: tokio::spawn(std::future::pending()),
            bridge: tokio::spawn(std::future::pending()),
            scheduled: tokio::spawn(std::future::pending()),
        };
        let mut workers = HashMap::from([
            ("removed".to_owned(), make_workers()),
            ("active".to_owned(), make_workers()),
        ]);
        let removed = workers.get("removed").unwrap().control.abort_handle();
        retain_active_workers(&mut workers, &HashSet::from(["active".to_owned()]));
        assert!(!workers.contains_key("removed"));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !removed.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("removed worker stops");
        assert!(workers.get("active").unwrap().is_running());
        workers.get("active").unwrap().abort();
    }

    #[tokio::test]
    async fn finished_worker_is_removed_so_it_can_restart() {
        let mut workers = HashMap::from([(
            "machine".to_owned(),
            MachineWorkers {
                control: tokio::spawn(async {}),
                bridge: tokio::spawn(std::future::pending()),
                scheduled: tokio::spawn(std::future::pending()),
            },
        )]);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !workers.get("machine").unwrap().control.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finished worker stops");
        retain_active_workers(&mut workers, &HashSet::from(["machine".to_owned()]));
        assert!(!workers.contains_key("machine"));
    }
}
