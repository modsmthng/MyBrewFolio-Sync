// SPDX-License-Identifier: GPL-3.0-or-later
//! Terminal interaction belongs to the CLI client, never to the background daemon.

use super::ControlRequest;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    future::Future,
    io::{self, BufRead, IsTerminal, Write},
    path::Path,
};

const NO_TERMINAL: &str = "notes enable requires an interactive terminal. Use notes activate-preview and notes activate <backup-id> <decisions.json> --confirm for scripted setup.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preview {
    backup_id: String,
    items: Vec<PreviewItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewItem {
    source_key: String,
    differs: bool,
}

pub(super) fn validate_decisions(value: &Value) -> Result<(), String> {
    let invalid = || {
        "Invalid Notes decisions. Use an array of unique sourceKey/resolution objects; resolution must be mybrewfolio or gaggimate.".to_string()
    };
    let entries = value.as_array().ok_or_else(invalid)?;
    if entries.len() > 5_000 {
        return Err(invalid());
    }
    let mut keys = HashSet::new();
    for entry in entries {
        let object = entry.as_object().ok_or_else(invalid)?;
        let key = entry["sourceKey"].as_str().ok_or_else(invalid)?;
        if object.len() != 2
            || key.trim().is_empty()
            || key.len() > 256
            || !keys.insert(key)
            || !matches!(
                entry["resolution"].as_str(),
                Some("mybrewfolio" | "gaggimate")
            )
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn request(args: &[&str], decisions: Option<Value>) -> ControlRequest {
    ControlRequest {
        command: "notes".into(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        decisions,
    }
}

fn answer(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
) -> Result<String, String> {
    write!(output, "{prompt}")
        .and_then(|_| output.flush())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    input.read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim().to_ascii_lowercase())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn export_custom(
    data_dir: &Path,
    decisions: &Value,
    backup_id: &str,
    install_dir: Option<&str>,
    output: &mut impl Write,
) -> Result<(), String> {
    let id = uuid::Uuid::new_v4();
    let directory = data_dir.join(format!("notes-activation-{id}"));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory).map_err(|e| e.to_string())?;
    let path = directory.join("decisions.json");
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(&mut file, decisions).map_err(|e| e.to_string())?;
    file.write_all(b"\n").map_err(|e| e.to_string())?;
    let install = install_dir
        .map(shell_quote)
        .unwrap_or_else(|| "\"$HOME/.config/mybrewfolio-sync\"".into());
    let compose = format!("docker compose --project-directory {install} -f {install}/compose.yaml");
    let local = format!("notes-decisions-{id}.json");
    let target = path.to_string_lossy();
    writeln!(output, "Custom decisions saved. MyBrewFolio is preselected for every difference.\nBackup ID: {backup_id}\nNotes Sync has NOT been enabled.\n\n1. Copy the file to your host:\n{compose} cp {} {local}\n\n2. Edit {local}: keep mybrewfolio or choose gaggimate for each sourceKey.\n\n3. Copy the reviewed file back as the container user (preserves private file permissions):\n{compose} exec -T sync sh -c 'cat > \"$1\"' sh {} < {local}\n\n4. Enable using your reviewed decisions:\n{install}/sync notes activate {} {} --confirm\n\nKeep the decision file private. Delete the host file and container decision directory when no longer needed.", shell_quote(&format!("sync:{target}")), shell_quote(&target), shell_quote(backup_id), shell_quote(&target)).map_err(|e| e.to_string())
}

async fn wizard<R, W, F, Fut>(
    interactive: bool,
    input: &mut R,
    output: &mut W,
    data_dir: &Path,
    install_dir: Option<&str>,
    mut call: F,
) -> Result<(), String>
where
    R: BufRead,
    W: Write,
    F: FnMut(ControlRequest) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    if !interactive {
        return Err(NO_TERMINAL.into());
    }
    writeln!(output, "Checking Notes Sync and creating the activation backup…\nWaiting for any current synchronization to finish.").map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())?;
    let value = call(request(&["enable-preview"], None)).await?;
    if value["alreadyEnabled"] == true {
        writeln!(
            output,
            "Two-way Notes Sync is already enabled for this installation."
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    // Deserialize only metadata. Machine/cloud Note contents never reach output or files.
    let preview: Preview = serde_json::from_value(value)
        .map_err(|_| "MyBrewFolio returned an invalid activation preview.".to_string())?;
    if uuid::Uuid::parse_str(&preview.backup_id).is_err() {
        return Err("MyBrewFolio returned an invalid activation backup ID.".into());
    }
    let mut decisions = Value::Array(
        preview
            .items
            .iter()
            .filter(|item| item.differs)
            .map(|item| json!({"sourceKey": item.source_key, "resolution": "mybrewfolio"}))
            .collect(),
    );
    validate_decisions(&decisions)?;
    let count = decisions.as_array().expect("array").len();
    writeln!(
        output,
        "Notes backup complete. {count} matching Brews have different Notes."
    )
    .map_err(|e| e.to_string())?;
    if count > 0 {
        loop {
            let choice = answer(input, output, "\n1. Keep MyBrewFolio Notes for all\n2. Use GaggiMate Notes for all\n3. Custom — choose individually using JSON\n4. Cancel\nChoose [1-4]: ")?;
            match choice.as_str() {
                "1" | "mybrewfolio" => break,
                "2" | "gaggimate" => {
                    for entry in decisions.as_array_mut().expect("array") {
                        entry["resolution"] = json!("gaggimate");
                    }
                    break;
                }
                "3" | "custom" => {
                    return export_custom(
                        data_dir,
                        &decisions,
                        &preview.backup_id,
                        install_dir,
                        output,
                    )
                }
                "4" | "cancel" | "" => {
                    writeln!(
                        output,
                        "Cancelled. Notes Sync has not been enabled; the backup is retained."
                    )
                    .map_err(|e| e.to_string())?;
                    return Ok(());
                }
                _ => writeln!(output, "Choose 1, 2, 3, or 4. No selection was applied.")
                    .map_err(|e| e.to_string())?,
            }
        }
        let side = if decisions[0]["resolution"] == "gaggimate" {
            "GaggiMate"
        } else {
            "MyBrewFolio"
        };
        writeln!(output, "\nUse {side} Notes for all {count} differences. Empty Notes on the selected side also replace Notes on the other side.").map_err(|e| e.to_string())?;
    } else {
        writeln!(
            output,
            "All matching Notes already agree. No initial overwrite is needed."
        )
        .map_err(|e| e.to_string())?;
    }
    let confirm = answer(input, output, "Enable two-way Notes Sync? [y/N] ")?;
    if !matches!(confirm.as_str(), "y" | "yes") {
        writeln!(
            output,
            "Cancelled. Notes Sync has not been enabled; the backup is retained."
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let result = call(request(
        &["activate", &preview.backup_id, "--confirm"],
        Some(decisions),
    ))
    .await?;
    if result["status"] != "two_way" {
        return Err(
            "MyBrewFolio did not confirm activation. Run sync status before retrying.".into(),
        );
    }
    writeln!(
        output,
        "Two-way Notes Sync is enabled. Review future Brew conflicts in MyBrewFolio."
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub(super) async fn run(socket: &std::path::PathBuf) -> Result<(), String> {
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    if !interactive {
        return Err(NO_TERMINAL.into());
    }
    #[cfg(unix)]
    {
        let install_dir = std::env::var("MYBREWFOLIO_SYNC_CLI_INSTALL_DIR").ok();
        wizard(true, &mut io::stdin().lock(), &mut io::stderr().lock(), &super::data_dir(), install_dir.as_deref(), |request| async move {
            super::proxy_control(socket, &request).await?.ok_or_else(|| "The Sync daemon is not running. Start the Docker service before running notes enable.".into())
        }).await
    }
    #[cfg(not(unix))]
    {
        let _ = socket;
        Err("Run notes enable inside the Linux Docker container.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    const BACKUP: &str = "6cf3cd65-f82a-4694-b68c-b6bc6fb16f85";
    fn preview(differs: bool) -> Value {
        json!({"backupId": BACKUP, "items": [{"sourceKey": "1:2", "differs": differs, "machineNotes": {"notes": "PRIVATE"}}]})
    }

    async fn simulate(
        input: &str,
        interactive: bool,
        response: Result<Value, String>,
    ) -> (
        Result<(), String>,
        String,
        Vec<ControlRequest>,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let mut requests = Vec::new();
        let mut output = Vec::new();
        let result = wizard(
            interactive,
            &mut Cursor::new(input),
            &mut output,
            directory.path(),
            Some("/host/with space"),
            |request| {
                let result = if requests.is_empty() {
                    response.clone()
                } else {
                    Ok(json!({"status": "two_way"}))
                };
                requests.push(request);
                std::future::ready(result)
            },
        )
        .await;
        (
            result,
            String::from_utf8(output).unwrap(),
            requests,
            directory,
        )
    }

    #[tokio::test]
    async fn bulk_choices_require_confirmation_and_never_write_files() {
        for (input, resolution) in [
            ("1\ny\n", "mybrewfolio"),
            ("2\nyes\n", "gaggimate"),
            ("oops\n1\ny\n", "mybrewfolio"),
        ] {
            let (result, output, calls, directory) = simulate(input, true, Ok(preview(true))).await;
            result.unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(
                calls[1].decisions,
                Some(json!([{"sourceKey":"1:2", "resolution":resolution}]))
            );
            assert_eq!(calls[1].args, ["activate", BACKUP, "--confirm"]);
            assert!(output.contains("Empty Notes"));
            assert!(!output.contains("PRIVATE"));
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn cancellation_eof_and_negative_confirmation_never_activate() {
        for input in ["", "4\n", "1\n", "1\nn\n", "2\ninvalid\n", "invalid\n"] {
            let (result, output, calls, _) = simulate(input, true, Ok(preview(true))).await;
            result.unwrap();
            assert_eq!(calls.len(), 1);
            assert!(output.contains("Cancelled"));
        }
    }

    #[tokio::test]
    async fn equal_notes_still_need_confirmation() {
        for (input, expected) in [("y\n", 2), ("\n", 1)] {
            let (result, output, calls, _) = simulate(input, true, Ok(preview(false))).await;
            result.unwrap();
            assert_eq!(calls.len(), expected);
            assert!(!output.contains("Choose [1-4]"));
            if expected == 2 {
                assert_eq!(calls[1].decisions, Some(json!([])));
            }
        }
    }

    #[tokio::test]
    async fn missing_terminal_errors_and_already_enabled_do_not_activate() {
        let (result, _, calls, _) = simulate("1\ny\n", false, Ok(preview(true))).await;
        assert!(result.unwrap_err().contains("interactive terminal"));
        assert!(calls.is_empty());
        for response in [
            Err("Backup failed".into()),
            Err("Another installation".into()),
            Ok(json!({"alreadyEnabled":true})),
            Ok(json!({"items":[]})),
        ] {
            let already_enabled = response
                .as_ref()
                .is_ok_and(|value| value["alreadyEnabled"] == true);
            let (result, output, calls, _) = simulate("1\ny\n", true, response).await;
            assert_eq!(result.is_ok(), already_enabled);
            assert_eq!(calls.len(), 1);
            assert!(!output.contains("Choose [1-4]"));
        }
    }

    #[tokio::test]
    async fn custom_exports_private_preselected_metadata_only() {
        let (result, output, calls, directory) = simulate("3\n", true, Ok(preview(true))).await;
        result.unwrap();
        assert_eq!(calls.len(), 1);
        let exported = fs::read_dir(directory.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let path = exported.join("decisions.json");
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!([{"sourceKey":"1:2", "resolution":"mybrewfolio"}])
        );
        assert!(!text.contains("PRIVATE"));
        assert!(!output.contains("PRIVATE"));
        assert!(output.contains("'/host/with space'/compose.yaml"));
        assert!(output.contains(BACKUP));
        assert!(output.contains("--confirm"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(exported).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn failed_or_unconfirmed_activation_is_not_reported_as_success() {
        for response in [Err("Activation failed".to_string()), Ok(json!({}))] {
            let directory = tempfile::tempdir().unwrap();
            let mut calls = 0;
            let mut output = Vec::new();
            let result = wizard(
                true,
                &mut Cursor::new("1\ny\n"),
                &mut output,
                directory.path(),
                None,
                |_| {
                    calls += 1;
                    std::future::ready(if calls == 1 {
                        Ok(preview(true))
                    } else {
                        response.clone()
                    })
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(calls, 2);
            assert!(!String::from_utf8(output)
                .unwrap()
                .contains("Two-way Notes Sync is enabled."));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wizard_sends_in_memory_decisions_over_the_existing_socket() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::UnixListener,
        };
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for value in [preview(true), json!({"status":"two_way"})] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                stream.read_to_end(&mut data).await.unwrap();
                requests.push(serde_json::from_slice::<ControlRequest>(&data).unwrap());
                stream
                    .write_all(
                        &serde_json::to_vec(&super::super::ControlResponse {
                            ok: true,
                            value: Some(value),
                            error: None,
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }
            requests
        });
        wizard(
            true,
            &mut Cursor::new("2\ny\n"),
            &mut Vec::new(),
            directory.path(),
            None,
            |request| {
                let socket = socket.clone();
                async move {
                    super::super::proxy_control(&socket, &request)
                        .await?
                        .ok_or_else(|| "Daemon missing".into())
                }
            },
        )
        .await
        .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests[0].args, ["enable-preview"]);
        assert!(requests[0].decisions.is_none());
        assert_eq!(
            requests[1].decisions,
            Some(json!([{"sourceKey":"1:2", "resolution":"gaggimate"}]))
        );
    }

    #[test]
    fn inline_decisions_are_strictly_validated() {
        for value in [
            json!({}),
            json!([{"sourceKey":"a", "resolution":"invalid"}]),
            json!([{"sourceKey":"a", "resolution":"mybrewfolio", "notes":"PRIVATE"}]),
            json!([{"sourceKey":"a", "resolution":"mybrewfolio"},{"sourceKey":"a", "resolution":"gaggimate"}]),
        ] {
            assert!(validate_decisions(&value).is_err());
        }
        assert!(validate_decisions(&json!([])).is_ok());
    }
}
