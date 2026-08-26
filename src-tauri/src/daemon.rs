// SPDX-License-Identifier: GPL-3.0-or-later

use std::{env, fs, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use mybrewfolio_sync_lib::{
    credentials::{CredentialStore, EncryptedFileCredentialStore},
    engine::SyncEngine,
    store::AppStore,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

fn data_dir() -> PathBuf {
    env::var_os("MYBREWFOLIO_SYNC_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data"))
}

fn key_path() -> Result<PathBuf, String> {
    env::var_os("MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE")
        .map(PathBuf::from)
        .ok_or_else(|| {
            "MYBREWFOLIO_SYNC_CREDENTIAL_KEY_FILE must point to a 32-byte Docker secret".into()
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
        Some("configure") => {
            "Usage: mybrewfolio-syncd configure <reuse-matching|import-all>\n\n\
             reuse-matching protects matching library entries from duplicate import."
        }
        Some("notes") => {
            "Usage: mybrewfolio-syncd notes <backup|activate-preview|activate|disable|restore-preview|restore>\n\n\
             Writing actions require their preview JSON and --confirm."
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
               diagnose                Show read-only JSON diagnostics and next steps\n\
               sync-once               Run one synchronization cycle\n\
               health                  Report container health\n\n\
             Setup and maintenance:\n\
               auth begin|wait         Pair this installation in a browser\n\
               host set <host>         Set the GaggiMate hostname, IP, or host:port\n\
               configure <policy>      Set reuse-matching or import-all\n\
               retry                   Retry failed local items\n\
               disconnect              Remove this installation's connection\n\n\
             Recovery (preview before writing):\n\
               notes <subcommand>      Back up, enable, restore, or disable Notes Sync\n\
               resync preview|apply    Review suppressed items; apply needs JSON and --confirm\n\n\
             Service:\n\
               daemon                  Run the continuous local synchronization service\n\n\
             Successful data commands write JSON to stdout. Logs and errors use stderr.\n\
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

async fn open_engine() -> Result<Arc<SyncEngine>, String> {
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
        SyncEngine::open(store, credentials).map_err(|error| error.to_string())?,
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
                match engine.poll_device_oauth().await {
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
            .sync_once()
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

#[cfg(unix)]
async fn serve_control(engine: Arc<SyncEngine>, socket: PathBuf) -> Result<(), String> {
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            return Err("another MyBrewFolio Sync daemon is already running".into());
        }
        fs::remove_file(&socket).map_err(|error| error.to_string())?;
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
                    match execute(&engine, &request.command, request.args).await {
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
    if command != "daemon" {
        #[cfg(unix)]
        match proxy_control(
            &socket,
            &ControlRequest {
                command: command.clone(),
                args: args.to_vec(),
            },
        )
        .await
        {
            Ok(Some(value)) => {
                print_json(value);
                return ExitCode::SUCCESS;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(1);
            }
        }
    }
    let engine = match open_engine().await {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(78);
        }
    };
    if command == "daemon" {
        #[cfg(unix)]
        {
            let control_engine = engine.clone();
            let control_socket = socket.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_control(control_engine, control_socket).await {
                    eprintln!("control socket error: {error}");
                }
            });
        }
        loop {
            if engine.status().await.connected {
                if let Err(error) = engine.sync_once().await {
                    eprintln!("sync error: {error}");
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
    match execute(&engine, command, args.to_vec()).await {
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
    use super::{confirmed, help_text, is_help_request, json_file};

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
}
