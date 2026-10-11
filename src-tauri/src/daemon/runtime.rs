// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(unix)]
use super::serve_control_multi;
use super::{
    daemon_error, first_sync_notice_due, should_log_sync_error, sync_attempt_message,
    sync_issue_message, MachineManager, MachineSyncError, SyncEngine,
    FIRST_SYNCHRONIZATION_MESSAGE,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::task::JoinHandle;

#[derive(Debug, PartialEq, Eq)]
enum SetupState {
    Pairing(String),
    Disconnected,
    MachineSelectionRequired,
    MachinesConnected(Vec<String>),
}

impl SetupState {
    fn message(&self) -> String {
        match self {
            Self::Pairing(url) => format!(
                "Connect MyBrewFolio: {url}\nOpen this link in your browser. It expires after 10 minutes. Waiting for account approval."
            ),
            Self::Disconnected =>
                "MyBrewFolio is disconnected. To reconnect, run inside this container:\n  mybrewfolio-syncd auth begin".into(),
            Self::MachineSelectionRequired =>
                "Account connected; machine selection required. Run inside this container:\n  mybrewfolio-syncd machines list\n  mybrewfolio-syncd machines add --existing MACHINE_ID HOST\nTo create a new machine instead:\n  mybrewfolio-syncd machines add NAME HOST\nReplace MACHINE_ID with the chosen account machine ID and HOST with its local GaggiMate address. Quote NAME if it contains spaces.".into(),
            Self::MachinesConnected(ids) => format!(
                "MyBrewFolio account and {} machine(s) connected. Finish setup or manage Sync at https://mybrewfolio.com/account/sync",
                ids.len()
            ),
        }
    }
}

#[derive(Default)]
struct SetupNotices {
    last: Option<SetupState>,
}

impl SetupNotices {
    fn observe(&mut self, outcome: Result<SetupState, String>) -> Result<Option<String>, String> {
        let state = outcome?;
        if self.last.as_ref() == Some(&state) {
            return Ok(None);
        }
        let message = state.message();
        self.last = Some(state);
        Ok(Some(message))
    }
}

async fn poll_setup(
    manager: &MachineManager,
    account_initialized: &mut bool,
) -> Result<SetupState, String> {
    let (url, connected) = manager
        .headless_pairing_and_authorize(Some("GaggiMate".into()), *account_initialized)
        .await?;
    *account_initialized = connected;
    if !connected {
        return Ok(match url {
            Some(url) => SetupState::Pairing(url),
            None => SetupState::Disconnected,
        });
    }
    let mut attached = Vec::new();
    for (id, engine) in manager.active_engines().await {
        if engine.status().await.connected {
            attached.push(id);
        }
    }
    attached.sort();
    Ok(if attached.is_empty() {
        SetupState::MachineSelectionRequired
    } else {
        SetupState::MachinesConnected(attached)
    })
}

async fn run_scheduled_machine(manager: Arc<MachineManager>, id: String, engine: Arc<SyncEngine>) {
    let mut first_sync_notice_printed = false;
    let mut logged_issues = HashMap::new();
    loop {
        let status = engine.status().await;
        if first_sync_notice_due(
            first_sync_notice_printed,
            status.connected,
            status.initial_sync_configured,
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
        let mut notices = SetupNotices::default();
        let mut account_initialized = false;
        loop {
            match notices.observe(poll_setup(&manager, &mut account_initialized).await) {
                Ok(Some(message)) => eprintln!("{message}"),
                Ok(None) => {}
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
    use super::{retain_active_workers, MachineWorkers, SetupNotices, SetupState};
    use std::{
        collections::{HashMap, HashSet},
        time::Duration,
    };

    #[test]
    fn pairing_and_registration_failure_never_advertise_sync_as_connected() {
        let mut notices = SetupNotices::default();
        let pairing = || SetupState::Pairing("https://example.test/pair".into());
        let first = notices.observe(Ok(pairing())).unwrap().unwrap();
        assert!(first.contains("https://example.test/pair"));
        assert!(!first.contains("/account/sync"));
        assert!(!first.contains("connected"));
        assert!(notices.observe(Ok(pairing())).unwrap().is_none());
        assert_eq!(
            notices.observe(Err("registration failed".into())),
            Err("registration failed".into())
        );
        assert!(notices.observe(Ok(pairing())).unwrap().is_none());
        let renewed = notices
            .observe(Ok(SetupState::Pairing(
                "https://example.test/renewed".into(),
            )))
            .unwrap()
            .unwrap();
        assert!(renewed.contains("https://example.test/renewed"));
    }

    #[test]
    fn machine_selection_and_cli_attachment_emit_their_next_steps_once() {
        let mut notices = SetupNotices::default();
        let selection = notices
            .observe(Ok(SetupState::MachineSelectionRequired))
            .unwrap()
            .unwrap();
        assert!(selection.contains("Account connected; machine selection required"));
        assert!(selection.contains("mybrewfolio-syncd machines list"));
        assert!(selection.contains("mybrewfolio-syncd machines add --existing MACHINE_ID HOST"));
        assert!(selection.contains("mybrewfolio-syncd machines add NAME HOST"));
        assert!(!selection.contains("/account/sync"));
        assert!(!selection.contains("docker compose exec sync"));
        assert!(notices
            .observe(Ok(SetupState::MachineSelectionRequired))
            .unwrap()
            .is_none());

        let attached = || SetupState::MachinesConnected(vec!["machine-1".into()]);
        let message = notices.observe(Ok(attached())).unwrap().unwrap();
        assert!(message.contains("1 machine(s) connected"));
        assert!(
            message.contains("Finish setup or manage Sync at https://mybrewfolio.com/account/sync")
        );
        assert!(notices.observe(Ok(attached())).unwrap().is_none());
        // A new daemon announces an already configured installation once as well.
        assert!(SetupNotices::default()
            .observe(Ok(attached()))
            .unwrap()
            .is_some());
        assert!(notices
            .observe(Ok(SetupState::MachineSelectionRequired))
            .unwrap()
            .is_some());
    }

    #[test]
    fn intentional_disconnect_requires_explicit_pairing_to_resume() {
        let mut notices = SetupNotices::default();
        let message = notices
            .observe(Ok(SetupState::Disconnected))
            .unwrap()
            .unwrap();
        assert!(message.contains("mybrewfolio-syncd auth begin"));
        assert!(!message.contains("/account/sync"));
        assert!(!message.contains("Waiting for account approval"));
        assert!(notices
            .observe(Ok(SetupState::Disconnected))
            .unwrap()
            .is_none());
        assert!(notices
            .observe(Ok(SetupState::Pairing("https://example.test/pair".into())))
            .unwrap()
            .is_some());
    }

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
