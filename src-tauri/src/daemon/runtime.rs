// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::collections::{HashMap, HashSet};
use tokio::task::JoinHandle;

async fn run_scheduled_machine(manager: Arc<MachineManager>, id: String, engine: Arc<SyncEngine>) {
    let mut first_sync_notice_printed = false;
    let mut logged_issues = HashMap::new();
    loop {
        let status = engine.status().await;
        if first_sync_notice_due(
            first_sync_notice_printed,
            status.connected,
            status.last_sync_at.as_deref(),
            status.last_error.as_deref(),
        ) {
            eprintln!("{FIRST_SYNCHRONIZATION_MESSAGE}");
            first_sync_notice_printed = true;
        }
        if status.connected {
            let result = manager.sync_machine_detailed(&id).await;
            let status = engine.status().await;
            match result {
                Ok(()) => {
                    for issue in status.issues {
                        let key = format!("{}:{}:{}", issue.kind, issue.source_key, issue.stage);
                        let marker = (issue.updated_at, issue.attempts);
                        if logged_issues.get(&key) != Some(&marker) {
                            daemon_error(&sync_issue_message(&issue, &status.machine_host));
                            logged_issues.insert(key, marker);
                        }
                    }
                }
                Err(MachineSyncError::Engine(error)) if should_log_sync_error(&error) => {
                    daemon_error(&format!(
                        "Machine {id}: {}",
                        sync_attempt_message(&error, &status.machine_host)
                    ));
                }
                Err(MachineSyncError::Unavailable(error)) => {
                    daemon_error(&format!("Machine {id}: {error}"))
                }
                Err(MachineSyncError::Engine(_)) => {}
            }
        }
        engine.wait_for_sync_interval().await;
    }
}

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

fn start_pairing_worker(manager: Arc<MachineManager>) {
    tokio::spawn(async move {
        let mut last_url = None;
        let mut account_initialized = false;
        loop {
            match manager
                .headless_pairing_and_authorize(Some("GaggiMate".into()), account_initialized)
                .await
            {
                Ok((url, connected)) => {
                    if url != last_url {
                        if let Some(url) = &url {
                            eprintln!("Connect MyBrewFolio: {url}\nOpen this link in your browser. It expires after 10 minutes.");
                        }
                        last_url = url;
                    }
                    account_initialized = connected;
                }
                Err(error) => {
                    daemon_error(&format!(
                        "Account connection unavailable: {error}. Retrying automatically."
                    ));
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

#[cfg(unix)]
fn start_control_socket(manager: Arc<MachineManager>, socket: PathBuf) {
    tokio::spawn(async move {
        if let Err(error) = serve_control_multi(manager, socket).await {
            daemon_error(&format!("Control socket error: {error}"));
        }
    });
}

async fn spawn_machine_workers(
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
    let (bridge_start, bridge_ready) = tokio::sync::oneshot::channel();
    let bridge = tokio::spawn(async move {
        if bridge_ready.await.is_err() {
            return;
        }
        loop {
            if bridge_engine.status().await.connected {
                if bridge_engine
                    .wait_for_profile_store_operations()
                    .await
                    .is_err()
                {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            } else {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    });
    let scheduled_manager = manager.clone();
    let scheduled_engine = engine.clone();
    let scheduled_id = id.to_owned();
    let (scheduled_start, scheduled_ready) = tokio::sync::oneshot::channel();
    let scheduled = tokio::spawn(async move {
        if scheduled_ready.await.is_ok() {
            run_scheduled_machine(scheduled_manager, scheduled_id, scheduled_engine).await;
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

pub(super) async fn run_daemon(manager: Arc<MachineManager>, socket: PathBuf) -> ! {
    start_pairing_worker(manager.clone());
    #[cfg(unix)]
    start_control_socket(manager.clone(), socket);
    #[cfg(not(unix))]
    let _ = socket;
    let mut workers = HashMap::new();
    loop {
        let active = manager.active_engines().await;
        let active_ids = active.iter().map(|(id, _)| id.clone()).collect();
        retain_active_workers(&mut workers, &active_ids);
        for (id, engine) in active {
            if workers.contains_key(&id) {
                continue;
            }
            let worker = spawn_machine_workers(&manager, &id, &engine).await;
            workers.insert(id, worker);
        }
        let _ = manager.flush_pending_detaches().await;
        let _ = manager.list_account_machines().await;
        let _ = manager.reconcile_auth_loss().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
