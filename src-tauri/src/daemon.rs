// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use chrono::{SecondsFormat, Utc};
use mybrewfolio_sync_lib::{
    credentials::{CredentialStore, EncryptedFileCredentialStore},
    engine::{EngineError, SyncEngine},
    local::LocalError,
    machines::{MachineManager, MachineSyncError},
    model::SyncIssue,
    store::AppStore,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

mod notes_wizard;

const FIRST_SYNCHRONIZATION_MESSAGE: &str =
    "Your first sync may take a while, depending on your history. You can leave this page and come back later. Keep the Sync app or Docker container running.";

fn first_sync_notice_due(
    announced: bool,
    connected: bool,
    last_sync_at: Option<&str>,
    last_error: Option<&str>,
) -> bool {
    !announced && connected && last_sync_at.is_none() && last_error.is_none()
}

fn should_log_sync_error(error: &EngineError) -> bool {
    !matches!(error, EngineError::Busy)
}

fn timestamped_log_line(timestamp: &str, message: &str) -> String {
    format!("{timestamp} {message}")
}

fn daemon_error(message: &str) {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    eprintln!("{}", timestamped_log_line(&timestamp, message));
}

fn sync_attempt_message(error: &EngineError, machine_host: &str) -> String {
    match error {
        EngineError::Local(LocalError::Unreachable) if machine_host == "gaggimate.local" => {
            "Sync attempt failed after its automatic retries: GaggiMate could not be reached. Sync will resume on the selected interval. Docker/NAS: gaggimate.local may not resolve inside containers. Set MYBREWFOLIO_SYNC_GAGGIMATE_HOST to GaggiMate's private LAN IP, then recreate the Sync container.".into()
        }
        EngineError::Local(LocalError::Unreachable) => {
            "Sync attempt failed after its automatic retries: GaggiMate could not be reached through the configured private LAN address. Sync will resume on the selected interval. Check that GaggiMate is online and reachable from the Docker or NAS network.".into()
        }
        EngineError::Local(LocalError::InvalidHost) => format!(
            "Sync configuration needs attention: {error}. Set MYBREWFOLIO_SYNC_GAGGIMATE_HOST to gaggimate.local or GaggiMate's private LAN IP, then recreate the Sync container."
        ),
        EngineError::Local(LocalError::UnsupportedShotFormat(_)) => format!(
            "Sync needs an update: {error}. Update MyBrewFolio Sync before retrying this shot."
        ),
        _ => format!(
            "Sync attempt failed after its automatic retries: {error}. Sync will resume on the selected interval."
        ),
    }
}

fn sync_issue_message(issue: &SyncIssue, machine_host: &str) -> String {
    let reason = issue.reason.trim_end();
    let punctuation = if reason.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    };
    let retry = if issue.reason.contains("retry automatically") {
        String::new()
    } else {
        " Retrying automatically.".into()
    };
    let docker_advice = if machine_host.eq_ignore_ascii_case("gaggimate.local")
        && issue.reason.contains("could not be reached")
    {
        " Docker/NAS: gaggimate.local may not resolve inside containers. Set MYBREWFOLIO_SYNC_GAGGIMATE_HOST to GaggiMate's private LAN IP, then recreate the Sync container."
    } else {
        ""
    };
    format!(
        "Sync item needs another attempt: {}{}{}{}",
        reason, punctuation, retry, docker_advice
    )
}

fn data_dir() -> PathBuf {
    env::var_os("MYBREWFOLIO_SYNC_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data"))
}

fn key_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE") {
        return Ok(PathBuf::from(path));
    }
    EncryptedFileCredentialStore::initialize_key(&data_dir()).map_err(|_| {
        "Cannot initialize the private state key. Check /data permissions. Existing credentials require their original key; mount it with MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE.".into()
    })
}

fn usage() -> &'static str {
    "Usage: mybrewfolio-syncd <command> [arguments]\n\
     Run 'mybrewfolio-syncd help' to list commands. Successful data commands write JSON to stdout."
}

fn help_text(args: &[String]) -> &'static str {
    let topic = match args {
        [first, topic, ..] if first == "help" => Some(topic.as_str()),
        [topic, flag, ..] if flag == "help" || flag == "--help" || flag == "-h" => {
            Some(topic.as_str())
        }
        _ => None,
    };
    match topic {
        Some("auth") => {
            "Usage: mybrewfolio-syncd auth <begin|wait>\n\n\
             auth begin  Create a short-lived browser pairing request.\n\
             auth wait   Wait for that request to be approved."
        }
        Some("host") => "Usage: mybrewfolio-syncd host set <hostname-or-ip>",
        Some("machines") => {
            "Usage: mybrewfolio-syncd machines <list|add <name> <host>|add --existing <id> <host>|rename <id> <name>|remove <id>>\n\n\
             Machine names contain 1–24 characters. Removing a machine disconnects only this installation; its MyBrewFolio history remains."
        }
        Some("configure") => {
            "Usage: mybrewfolio-syncd configure <reuse-matching|import-all>\n\n\
             reuse-matching protects matching library entries from duplicate import."
        }
        Some("notes") => {
            "Usage: mybrewfolio-syncd notes <enable|backup|activate-preview|activate|disable|restore-preview|restore>\n\n\
             notes enable  Interactively back up, review, and enable two-way Notes Sync.\n\
             notes activate <backup-id> <decisions.json> --confirm  Apply reviewed custom choices.\n\
             Other writing actions require their preview JSON and --confirm."
        }
        Some("resync") => {
            "Usage: mybrewfolio-syncd resync <preview|apply decisions.json --confirm>\n\n\
             Preview first. Apply accepts only an explicit decisions JSON file and --confirm."
        }
        _ => {
            "MyBrewFolio Sync daemon\n\n\
             Usage: mybrewfolio-syncd <command> [arguments]\n\n\
             Everyday commands:\n\
               help, --help, -h       Show this help without starting the daemon\n\
               status                  Show the current synchronization status as JSON\n\
               machines list           List machines in this account\n\
               diagnose                Show read-only JSON diagnostics and next steps\n\
               sync-once               Run one synchronization cycle\n\
               health                  Report container health\n\n\
             Setup and maintenance:\n\
               auth begin|wait         Pair this installation in a browser\n\
               machines add <name> <host>  Add a new named GaggiMate machine\n\
               machines add --existing <id> <host>  Connect an existing machine\n\
               machines rename|remove <id> ...  Manage a connected machine\n\
               host set <host>         Set the GaggiMate hostname, IP, or host:port\n\
               configure <policy>      Set reuse-matching or import-all\n\
               retry                   Retry failed local items\n\
               disconnect              Remove this installation's connection\n\n\
             Recovery (preview before writing):\n\
               notes enable            Interactive two-way Notes Sync setup\n\
               notes <subcommand>      Back up, restore, or disable Notes Sync\n\
               resync preview|apply    Review suppressed items; apply needs JSON and --confirm\n\n\
             Service:\n\
               daemon                  Run the continuous local synchronization service\n\n\
             Successful data commands write JSON to stdout. Logs and errors use stderr.\n\
             Use --machine <id> for machine-specific commands when several are connected.\n\
             sync-once without --machine synchronizes all connected machines.\n\
             Configure MYBREWFOLIO_SYNC_DATA_DIR and MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE."
        }
    }
}

fn is_help_request(args: &[String]) -> bool {
    args.is_empty()
        || matches!(
            args.first().map(String::as_str),
            Some("help" | "--help" | "-h")
        )
        || matches!(
            args.get(1).map(String::as_str),
            Some("help" | "--help" | "-h")
        )
}

fn print_json(value: serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string(&value).expect("JSON serialization")
    );
}

#[derive(Serialize, Deserialize)]
struct ControlRequest {
    command: String,
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decisions: Option<Value>,
}

#[derive(Serialize, Deserialize)]
struct ControlResponse {
    ok: bool,
    value: Option<Value>,
    error: Option<String>,
}

fn json_file(path: Option<String>) -> Result<serde_json::Value, String> {
    let path = path.ok_or_else(|| "a JSON file path is required".to_string())?;
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&contents).map_err(|error| format!("invalid JSON: {error}"))
}

fn confirmed(value: Option<String>) -> Result<(), String> {
    if value.as_deref() == Some("--confirm") {
        Ok(())
    } else {
        Err("this operation requires --confirm".into())
    }
}

async fn open_manager() -> Result<Arc<MachineManager>, String> {
    let data = data_dir();
    let store =
        Arc::new(AppStore::open(&data.join("sync.sqlite")).map_err(|error| error.to_string())?);
    if let Some(host) = env::var_os("MYBREWFOLIO_SYNC_GAGGIMATE_HOST") {
        store
            .set_setting("machine_host", &host.to_string_lossy())
            .map_err(|error| error.to_string())?;
    }
    let credentials: Arc<dyn CredentialStore> = Arc::new(
        EncryptedFileCredentialStore::from_key_file(data.join("credentials.enc"), &key_path()?)
            .map_err(|error| error.to_string())?,
    );
    Ok(Arc::new(
        MachineManager::open(&data, store, credentials).map_err(|error| error.to_string())?,
    ))
}

async fn execute_auth(
    engine: &SyncEngine,
    mut args: impl Iterator<Item = String>,
) -> Result<Value, String> {
    match args.next().as_deref() {
        Some("begin") => {
            let info = engine
                .begin_device_oauth()
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_value(info).map_err(|error| error.to_string())
        }
        Some("wait") => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
            loop {
                match engine.poll_device_oauth_session().await {
                    Ok(true) => break Ok(json!({"ok": true, "connected": true})),
                    Ok(false) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_secs(5)).await
                    }
                    Ok(false) => {
                        break Err("Device authorization timed out; run auth begin again".into())
                    }
                    Err(error) => break Err(error.to_string()),
                }
            }
        }
        _ => Err("Usage: mybrewfolio-syncd auth <begin|wait>".into()),
    }
}

async fn execute_auth_multi(
    manager: &MachineManager,
    mut args: impl Iterator<Item = String>,
) -> Result<Value, String> {
    match args.next().as_deref() {
        Some("begin") => {
            let _account = manager.account_operation().await;
            let info = manager
                .primary()
                .begin_device_oauth()
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_value(info).map_err(|error| error.to_string())
        }
        Some("wait") => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
            loop {
                match manager
                    .poll_device_oauth_and_authorize(Some("GaggiMate".into()))
                    .await
                {
                    Ok(true) => break Ok(json!({"ok": true, "connected": true})),
                    Ok(false) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_secs(5)).await
                    }
                    Ok(false) => {
                        break Err("Device authorization timed out; run auth begin again".into())
                    }
                    Err(error) => break Err(error),
                }
            }
        }
        _ => Err("Usage: mybrewfolio-syncd auth <begin|wait>".into()),
    }
}

async fn execute_host(
    engine: &SyncEngine,
    mut args: impl Iterator<Item = String>,
) -> Result<Value, String> {
    match (args.next().as_deref(), args.next()) {
        (Some("set"), Some(host)) => engine
            .set_host(&host)
            .await
            .map(|_| json!({"ok": true, "host": host}))
            .map_err(|error| error.to_string()),
        _ => Err("Usage: mybrewfolio-syncd host set <host>".into()),
    }
}

async fn execute_configure(
    engine: &SyncEngine,
    mut args: impl Iterator<Item = String>,
) -> Result<Value, String> {
    match args.next().as_deref() {
        Some("reuse-matching") => engine
            .configure_sync(true)
            .await
            .map(|_| json!({"ok": true, "duplicatePolicy": "reuse_matching"}))
            .map_err(|error| error.to_string()),
        Some("import-all") => engine
            .configure_sync(false)
            .await
            .map(|_| json!({"ok": true, "duplicatePolicy": "import_all"}))
            .map_err(|error| error.to_string()),
        _ => Err("Usage: mybrewfolio-syncd configure <reuse-matching|import-all>".into()),
    }
}

async fn execute_notes(
    engine: &SyncEngine,
    mut args: impl Iterator<Item = String>,
) -> Result<Value, String> {
    match args.next().as_deref() {
        Some("backup") => engine
            .create_latest_notes_backup()
            .await
            .map(|backup_id| json!({"backupId": backup_id}))
            .map_err(|error| error.to_string()),
        Some("activate-preview") => engine
            .begin_two_way_notes_activation()
            .await
            .map_err(|error| error.to_string()),
        Some("activate") => {
            let backup_id = args.next().ok_or_else(|| {
                "notes activate requires <backup-id> <decisions.json> --confirm".to_string()
            })?;
            let decisions = json_file(args.next())?;
            confirmed(args.next())?;
            engine
                .activate_two_way_notes(&backup_id, decisions)
                .await
                .map_err(|error| error.to_string())
        }
        Some("disable") => {
            confirmed(args.next())?;
            engine
                .disable_two_way_notes()
                .await
                .map(|_| json!({"ok": true}))
                .map_err(|error| error.to_string())
        }
        Some("restore-preview") => {
            let backup_id = args
                .next()
                .ok_or_else(|| "notes restore-preview requires <backup-id>".to_string())?;
            engine
                .preview_notes_restore(&backup_id)
                .await
                .map_err(|error| error.to_string())
        }
        Some("restore") => {
            let backup_id = args.next().ok_or_else(|| {
                "notes restore requires <backup-id> <source-keys.json> --confirm".to_string()
            })?;
            let source_keys: Vec<String> = serde_json::from_value(json_file(args.next())?)
                .map_err(|error| format!("source keys must be a JSON array of strings: {error}"))?;
            confirmed(args.next())?;
            engine
                .restore_notes_backup(&backup_id, &source_keys)
                .await
                .map_err(|error| error.to_string())
        }
        _ => Err(
            "Usage: notes <backup|activate-preview|activate|disable|restore-preview|restore>"
                .into(),
        ),
    }
}

async fn execute_resync(
    engine: &SyncEngine,
    mut args: impl Iterator<Item = String>,
) -> Result<Value, String> {
    match args.next().as_deref() {
        Some("preview") => engine
            .resync_preview()
            .await
            .map_err(|error| error.to_string()),
        Some("apply") => {
            let decisions = json_file(args.next())?;
            confirmed(args.next())?;
            engine
                .apply_resync(decisions)
                .await
                .map_err(|error| error.to_string())
        }
        _ => Err("Usage: resync <preview|apply decisions.json --confirm>".into()),
    }
}

async fn execute(
    engine: &SyncEngine,
    command: &str,
    arguments: Vec<String>,
) -> Result<Value, String> {
    match command {
        "status" => Ok(serde_json::to_value(engine.status().await).expect("status JSON")),
        "diagnose" => engine.diagnose().await.map_err(|error| error.to_string()),
        "health" => Ok(json!({"ok": true})),
        "auth" => execute_auth(engine, arguments.into_iter()).await,
        "sync-once" => engine
            .sync_with_retries()
            .await
            .map(|_| json!({"ok": true}))
            .map_err(|error| error.to_string()),
        "host" => execute_host(engine, arguments.into_iter()).await,
        "configure" => execute_configure(engine, arguments.into_iter()).await,
        "notes" => execute_notes(engine, arguments.into_iter()).await,
        "resync" => execute_resync(engine, arguments.into_iter()).await,
        "retry" => engine
            .retry_failures()
            .await
            .map(|_| json!({"ok": true}))
            .map_err(|error| error.to_string()),
        "disconnect" => engine.disconnect().await.map_err(|error| error.to_string()),
        _ => Err(usage().into()),
    }
}

fn take_machine_selector(arguments: Vec<String>) -> Result<(Vec<String>, Option<String>), String> {
    let mut args = Vec::new();
    let mut selected = None;
    let mut iter = arguments.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "--machine" {
            if selected.is_some() {
                return Err("--machine may be specified only once".into());
            }
            selected = Some(iter.next().ok_or("--machine requires a machine ID")?);
        } else {
            args.push(arg);
        }
    }
    Ok((args, selected))
}

async fn execute_multi(
    manager: &Arc<MachineManager>,
    command: &str,
    arguments: Vec<String>,
) -> Result<Value, String> {
    let (arguments, machine_id) = take_machine_selector(arguments)?;
    match command {
        "status" if machine_id.is_none() => Ok(manager.status_json().await),
        "machines" => {
            if machine_id.is_some() {
                return Err("Use the machine ID as an argument to machines".into());
            }
            match arguments.as_slice() {
                [action] if action == "list" => {
                    Ok(json!({"machines": manager.list_account_machines().await?}))
                }
                [action, name, host] if action == "add" => {
                    let id = manager.add_machine(name, host).await?;
                    Ok(json!({"ok": true, "machineId": id}))
                }
                [action, flag, id, host] if action == "add" && flag == "--existing" => {
                    manager.connect_machine(id, host).await?;
                    Ok(json!({"ok": true, "machineId": id}))
                }
                [action, id, name] if action == "rename" => {
                    manager.rename_machine(id, name).await?;
                    Ok(json!({"ok": true, "machineId": id, "name": name}))
                }
                [action, id] if action == "remove" => {
                    manager.remove_machine(id).await?;
                    Ok(json!({"ok": true, "machineId": id}))
                }
                _ => Err("Usage: machines <list|add <name> <host>|add --existing <id> <host>|rename <id> <name>|remove <id>>".into()),
            }
        }
        "auth" => execute_auth_multi(manager, arguments.into_iter()).await,
        "disconnect" => manager.disconnect_all().await,
        "sync-once" if machine_id.is_none() => {
            let outcomes = manager.sync_all().await;
            let failed: Vec<_> = outcomes
                .into_iter()
                .filter_map(|(id, result)| {
                    result
                        .err()
                        .map(|error| json!({"machineId": id, "error": error}))
                })
                .collect();
            if failed.is_empty() {
                Ok(json!({"ok": true}))
            } else {
                Err(format!(
                    "One or more machines could not synchronize: {}",
                    Value::Array(failed)
                ))
            }
        }
        "sync-once" => {
            manager.sync_selected(machine_id.as_deref()).await?;
            Ok(json!({"ok": true}))
        }
        "health" => Ok(json!({"ok": true})),
        _ => {
            let _account = manager.account_operation().await;
            let engine = manager.selected_engine(machine_id.as_deref()).await?;
            execute(&engine, command, arguments).await
        }
    }
}

async fn execute_control_multi(
    manager: &Arc<MachineManager>,
    mut request: ControlRequest,
) -> Result<Value, String> {
    let (arguments, machine_id) = take_machine_selector(request.args)?;
    request.args = arguments;
    if let Some(decisions) = request.decisions {
        if request.command != "notes"
            || request.args.len() != 3
            || request.args[0] != "activate"
            || request.args[2] != "--confirm"
        {
            return Err("Inline decisions require notes activate <backup-id> --confirm.".into());
        }
        notes_wizard::validate_decisions(&decisions)?;
        let _account = manager.account_operation().await;
        let engine = manager.selected_engine(machine_id.as_deref()).await?;
        return engine
            .activate_headless_notes(&request.args[1], decisions)
            .await;
    }
    if request.command == "notes" && request.args == ["enable-preview"] {
        let _account = manager.account_operation().await;
        let engine = manager.selected_engine(machine_id.as_deref()).await?;
        return engine.prepare_headless_notes_activation().await;
    }
    let mut args = request.args;
    if let Some(machine_id) = machine_id {
        args.push("--machine".into());
        args.push(machine_id);
    }
    execute_multi(manager, &request.command, args).await
}

async fn execute_control(engine: &SyncEngine, request: ControlRequest) -> Result<Value, String> {
    if let Some(decisions) = request.decisions {
        if request.command != "notes"
            || request.args.len() != 3
            || request.args[0] != "activate"
            || request.args[2] != "--confirm"
        {
            return Err("Inline decisions require notes activate <backup-id> --confirm.".into());
        }
        notes_wizard::validate_decisions(&decisions)?;
        return engine
            .activate_headless_notes(&request.args[1], decisions)
            .await;
    }
    if request.command == "notes" && request.args == ["enable-preview"] {
        return engine.prepare_headless_notes_activation().await;
    }
    execute(engine, &request.command, request.args).await
}

#[cfg(unix)]
async fn serve_control(engine: Arc<SyncEngine>, socket: PathBuf) -> Result<(), String> {
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            return Err("another MyBrewFolio Sync daemon is already running".into());
        }
        let stale_socket = socket.clone();
        tokio::task::spawn_blocking(move || fs::remove_file(stale_socket))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
    }
    let listener = UnixListener::bind(&socket).map_err(|error| error.to_string())?;
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let engine = engine.clone();
        tokio::spawn(async move {
            let mut input = Vec::new();
            let _ = stream.read_to_end(&mut input).await;
            let response = match serde_json::from_slice::<ControlRequest>(&input) {
                Ok(request) if request.command != "daemon" => {
                    match execute_control(&engine, request).await {
                        Ok(value) => ControlResponse {
                            ok: true,
                            value: Some(value),
                            error: None,
                        },
                        Err(error) => ControlResponse {
                            ok: false,
                            value: None,
                            error: Some(error),
                        },
                    }
                }
                Ok(_) => ControlResponse {
                    ok: false,
                    value: None,
                    error: Some("daemon cannot be nested".into()),
                },
                Err(_) => ControlResponse {
                    ok: false,
                    value: None,
                    error: Some("invalid local control request".into()),
                },
            };
            let _ = stream
                .write_all(&serde_json::to_vec(&response).expect("control JSON"))
                .await;
        });
    }
}

#[cfg(unix)]
async fn serve_control_multi(manager: Arc<MachineManager>, socket: PathBuf) -> Result<(), String> {
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            return Err("another MyBrewFolio Sync daemon is already running".into());
        }
        let stale_socket = socket.clone();
        tokio::task::spawn_blocking(move || fs::remove_file(stale_socket))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
    }
    let listener = UnixListener::bind(&socket).map_err(|error| error.to_string())?;
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
        let manager = manager.clone();
        tokio::spawn(async move {
            let mut input = Vec::new();
            let _ = stream.read_to_end(&mut input).await;
            let response = match serde_json::from_slice::<ControlRequest>(&input) {
                Ok(request) if request.command != "daemon" => {
                    match execute_control_multi(&manager, request).await {
                        Ok(value) => ControlResponse {
                            ok: true,
                            value: Some(value),
                            error: None,
                        },
                        Err(error) => ControlResponse {
                            ok: false,
                            value: None,
                            error: Some(error),
                        },
                    }
                }
                Ok(_) => ControlResponse {
                    ok: false,
                    value: None,
                    error: Some("daemon cannot be nested".into()),
                },
                Err(_) => ControlResponse {
                    ok: false,
                    value: None,
                    error: Some("invalid local control request".into()),
                },
            };
            let _ = stream
                .write_all(&serde_json::to_vec(&response).expect("control JSON"))
                .await;
        });
    }
}

#[cfg(unix)]
async fn proxy_control(
    socket: &PathBuf,
    request: &ControlRequest,
) -> Result<Option<Value>, String> {
    let mut stream = match UnixStream::connect(socket).await {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    stream
        .write_all(&serde_json::to_vec(request).expect("control JSON"))
        .await
        .map_err(|error| error.to_string())?;
    stream.shutdown().await.map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    stream
        .read_to_end(&mut output)
        .await
        .map_err(|error| error.to_string())?;
    let response: ControlResponse = serde_json::from_slice(&output)
        .map_err(|_| "invalid local control response".to_string())?;
    if response.ok {
        Ok(response.value)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "local control failed".into()))
    }
}

async fn run_notes_wizard(command: &str, args: &[String], socket: &PathBuf) -> Option<ExitCode> {
    if command == "notes" && args.first().map(String::as_str) == Some("enable") {
        let (selected_args, machine_id) = match take_machine_selector(args.to_vec()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("{error}");
                return Some(ExitCode::from(64));
            }
        };
        if selected_args != ["enable"] {
            eprintln!("Usage: mybrewfolio-syncd notes enable [--machine <id>]");
            return Some(ExitCode::from(64));
        }
        return Some(
            match notes_wizard::run(socket, machine_id.as_deref()).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(1)
                }
            },
        );
    }
    None
}

#[cfg(unix)]
async fn proxy_active_daemon(socket: &PathBuf, command: &str, args: &[String]) -> Option<ExitCode> {
    match proxy_control(
        socket,
        &ControlRequest {
            command: command.to_string(),
            args: args.to_vec(),
            decisions: None,
        },
    )
    .await
    {
        Ok(Some(value)) => {
            print_json(value);
            Some(ExitCode::SUCCESS)
        }
        Ok(None) => None,
        Err(error) => {
            eprintln!("{error}");
            Some(ExitCode::from(1))
        }
    }
}

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

async fn run_daemon(manager: Arc<MachineManager>, socket: PathBuf) -> ! {
    let pairing_manager = manager.clone();
    tokio::spawn(async move {
        let mut last_url = None;
        let mut account_initialized = false;
        loop {
            match pairing_manager
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
    #[cfg(unix)]
    {
        let control_manager = manager.clone();
        tokio::spawn(async move {
            if let Err(error) = serve_control_multi(control_manager, socket).await {
                daemon_error(&format!("Control socket error: {error}"));
            }
        });
    }
    let mut workers: HashMap<
        String,
        (
            tokio::task::JoinHandle<()>,
            tokio::task::JoinHandle<()>,
            tokio::task::JoinHandle<()>,
        ),
    > = HashMap::new();
    loop {
        let active = manager.active_engines().await;
        let active_ids: HashSet<_> = active.iter().map(|(id, _)| id.clone()).collect();
        workers.retain(|id, (control, bridge, scheduled)| {
            if active_ids.contains(id)
                && !control.is_finished()
                && !bridge.is_finished()
                && !scheduled.is_finished()
            {
                true
            } else {
                control.abort();
                bridge.abort();
                scheduled.abort();
                false
            }
        });
        for (id, engine) in &active {
            if workers.contains_key(id) {
                continue;
            }
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
            let scheduled_id = id.clone();
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
            workers.insert(id.clone(), (control, bridge, scheduled));
        }
        let _ = manager.flush_pending_detaches().await;
        let _ = manager.list_account_machines().await;
        let _ = manager.reconcile_auth_loss().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let all_args: Vec<String> = env::args().skip(1).collect();
    if is_help_request(&all_args) {
        println!("{}", help_text(&all_args));
        return ExitCode::SUCCESS;
    }
    let Some((command, args)) = all_args.split_first() else {
        unreachable!("an empty command was handled as a help request");
    };
    let socket = data_dir().join("control.sock");
    if let Some(exit_code) = run_notes_wizard(command, args, &socket).await {
        return exit_code;
    }
    if command != "daemon" {
        #[cfg(unix)]
        if let Some(exit_code) = proxy_active_daemon(&socket, command, args).await {
            return exit_code;
        }
    }
    let manager = match open_manager().await {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(78);
        }
    };
    if command == "daemon" {
        run_daemon(manager, socket).await;
    }
    match execute_multi(&manager, command, args.to_vec()).await {
        Ok(value) => {
            print_json(value);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        confirmed, execute, execute_control, first_sync_notice_due, help_text, is_help_request,
        json_file, should_log_sync_error, sync_attempt_message, sync_issue_message,
        timestamped_log_line, ControlRequest, EngineError, SyncEngine,
        FIRST_SYNCHRONIZATION_MESSAGE,
    };
    #[cfg(unix)]
    use super::{proxy_control, serve_control, ControlResponse};
    use mybrewfolio_sync_lib::{
        credentials::EncryptedFileCredentialStore, local::LocalError, model::SyncIssue,
        store::AppStore,
    };

    fn test_engine() -> (Arc<SyncEngine>, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let key = directory.path().join("key");
        std::fs::write(&key, [7_u8; 32]).expect("key written");
        let store = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).expect("store"));
        let credentials = Arc::new(
            EncryptedFileCredentialStore::from_key_file(
                directory.path().join("credentials.enc"),
                &key,
            )
            .expect("credentials store"),
        );
        (
            Arc::new(SyncEngine::open(store, credentials).expect("engine")),
            directory,
        )
    }

    #[test]
    fn first_sync_notice_is_emitted_once_without_an_error() {
        assert_eq!(
            FIRST_SYNCHRONIZATION_MESSAGE,
            "Your first sync may take a while, depending on your history. You can leave this page and come back later. Keep the Sync app or Docker container running."
        );
        assert!(first_sync_notice_due(false, true, None, None));
        assert!(!first_sync_notice_due(true, true, None, None));
        assert!(!first_sync_notice_due(
            false,
            true,
            Some("2026-09-16T00:00:00Z"),
            None
        ));
        assert!(!first_sync_notice_due(
            false,
            true,
            None,
            Some("The GaggiMate could not be reached")
        ));
    }

    #[test]
    fn expected_busy_syncs_are_not_written_as_errors() {
        assert!(!should_log_sync_error(&EngineError::Busy));
    }

    #[test]
    fn retry_logs_are_timestamped_and_include_docker_guidance() {
        let message = sync_attempt_message(
            &EngineError::Local(LocalError::Unreachable),
            "gaggimate.local",
        );
        assert_eq!(
            timestamped_log_line("2026-09-18T14:30:00Z", &message),
            "2026-09-18T14:30:00Z Sync attempt failed after its automatic retries: GaggiMate could not be reached. Sync will resume on the selected interval. Docker/NAS: gaggimate.local may not resolve inside containers. Set MYBREWFOLIO_SYNC_GAGGIMATE_HOST to GaggiMate's private LAN IP, then recreate the Sync container."
        );
    }

    #[test]
    fn retry_logs_keep_private_ips_out_of_messages_and_name_invalid_data_context() {
        let unreachable =
            sync_attempt_message(&EngineError::Local(LocalError::Unreachable), "192.168.1.42");
        assert!(unreachable.contains("configured private LAN address"));
        assert!(!unreachable.contains("192.168.1.42"));
        assert!(unreachable.contains("selected interval"));

        let notes = sync_attempt_message(
            &EngineError::Local(LocalError::InvalidNotes(123)),
            "gaggimate.local",
        );
        assert_eq!(
            notes,
            "Sync attempt failed after its automatic retries: The GaggiMate returned invalid data while reading Notes for shot 123. Sync will resume on the selected interval."
        );

        assert_eq!(
            sync_issue_message(
                &SyncIssue {
                    kind: "notes".into(),
                    source_key: "123:456".into(),
                    stage: "read".into(),
                    reason: "The GaggiMate returned invalid data while reading Notes for shot 123"
                        .into(),
                    attempts: 1,
                    updated_at: 0,
                },
                "gaggimate.local",
            ),
            "Sync item needs another attempt: The GaggiMate returned invalid data while reading Notes for shot 123. Retrying automatically."
        );
    }

    #[test]
    fn help_is_available_without_a_running_daemon() {
        assert!(is_help_request(&[]));
        assert!(is_help_request(&["diagnose".into(), "--help".into()]));
        let help = help_text(&["help".into()]);
        assert!(help.contains("diagnose"));
        assert!(help.contains("resync preview|apply"));
    }

    #[test]
    fn grouped_help_describes_pairing() {
        let help = help_text(&["auth".into(), "-h".into()]);
        assert!(help.contains("auth begin"));
        assert!(help.contains("auth wait"));
    }

    #[test]
    fn every_command_group_has_help_without_opening_the_database() {
        for topic in ["host", "configure", "notes", "resync"] {
            let help = help_text(&[topic.into(), "--help".into()]);
            assert!(help.starts_with("Usage:"), "missing help for {topic}");
        }
        assert!(help_text(&["unknown".into()]).contains("MyBrewFolio Sync daemon"));
    }

    #[test]
    fn destructive_commands_require_the_explicit_confirmation_flag() {
        assert!(confirmed(Some("--confirm".into())).is_ok());
        assert!(confirmed(None).is_err());
        assert!(confirmed(Some("confirm".into())).is_err());
    }

    #[test]
    fn json_file_reports_missing_and_invalid_decision_files() {
        assert!(json_file(None).is_err());
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("decisions.json");
        std::fs::write(&path, r#"{"restoreItemIds":["one"]}"#).expect("decision file written");
        assert_eq!(
            json_file(Some(path.to_string_lossy().into_owned())).expect("JSON read")
                ["restoreItemIds"][0],
            "one"
        );
        let invalid = directory.path().join("invalid.json");
        std::fs::write(&invalid, "not json").expect("invalid file written");
        assert!(json_file(Some(invalid.to_string_lossy().into_owned())).is_err());
    }

    #[tokio::test]
    async fn command_dispatch_keeps_read_only_and_validation_contracts_stable() {
        let (engine, _directory) = test_engine();

        assert_eq!(
            execute(&engine, "health", vec![])
                .await
                .expect("health response")["ok"],
            true
        );
        assert_eq!(
            execute(&engine, "host", vec!["set".into(), "127.0.0.1:8088".into()])
                .await
                .expect("host response")["host"],
            "127.0.0.1:8088"
        );
        assert_eq!(
            execute(&engine, "status", vec![])
                .await
                .expect("status response")["machineHost"],
            "127.0.0.1:8088"
        );
        assert!(execute(&engine, "configure", vec!["unexpected".into()])
            .await
            .expect_err("invalid policy")
            .contains("Usage:"));
        assert!(execute(&engine, "notes", vec!["disable".into()])
            .await
            .expect_err("confirmation required")
            .contains("--confirm"));
        assert!(execute(&engine, "resync", vec!["apply".into()])
            .await
            .expect_err("decision file required")
            .contains("JSON file"));
        assert!(execute(&engine, "unknown", vec![])
            .await
            .expect_err("unknown command")
            .contains("Usage:"));
    }

    #[tokio::test]
    async fn control_requests_preserve_legacy_shape_and_validate_inline_confirmation() {
        let (engine, _directory) = test_engine();
        let legacy: ControlRequest =
            serde_json::from_value(serde_json::json!({"command":"health", "args":[]})).unwrap();
        assert!(legacy.decisions.is_none());
        assert_eq!(execute_control(&engine, legacy).await.unwrap()["ok"], true);
        for (args, decisions, expected) in [
            (
                vec!["activate", "backup"],
                serde_json::json!([]),
                "--confirm",
            ),
            (
                vec!["activate", "backup", "--confirm"],
                serde_json::json!({}),
                "Invalid Notes decisions",
            ),
        ] {
            let result = execute_control(
                &engine,
                ControlRequest {
                    command: "notes".into(),
                    args: args.into_iter().map(str::to_string).collect(),
                    decisions: Some(decisions),
                },
            )
            .await;
            assert!(result.unwrap_err().contains(expected));
        }
        assert!(
            execute(&engine, "notes", vec!["enable".into()])
                .await
                .is_err(),
            "a background daemon must never prompt"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_control_socket_proxies_requests_and_rejects_invalid_input() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::UnixStream,
        };

        let (engine, directory) = test_engine();
        let socket = directory.path().join("control.sock");
        assert!(proxy_control(
            &socket,
            &ControlRequest {
                command: "health".into(),
                args: vec![],
                decisions: None,
            }
        )
        .await
        .expect("missing daemon is not an error")
        .is_none());

        let task = tokio::spawn(serve_control(engine, socket.clone()));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !socket.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("control socket starts");

        let health = proxy_control(
            &socket,
            &ControlRequest {
                command: "health".into(),
                args: vec![],
                decisions: None,
            },
        )
        .await
        .expect("proxy succeeds")
        .expect("daemon responded");
        assert_eq!(health["ok"], true);

        let mut stream = UnixStream::connect(&socket).await.expect("socket connects");
        stream
            .write_all(b"not JSON")
            .await
            .expect("invalid request written");
        stream.shutdown().await.expect("request complete");
        let mut output = Vec::new();
        stream
            .read_to_end(&mut output)
            .await
            .expect("response read");
        let response: ControlResponse = serde_json::from_slice(&output).expect("JSON response");
        assert!(!response.ok);
        assert_eq!(
            response.error.as_deref(),
            Some("invalid local control request")
        );

        task.abort();
    }
}
