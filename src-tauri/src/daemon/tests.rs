// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Arc;

use super::{
    confirmed, execute, execute_control, first_sync_notice_due, help_text, is_help_request,
    json_file, should_log_sync_error, sync_attempt_message, sync_issue_message,
    timestamped_log_line, ControlRequest, EngineError, SyncEngine, FIRST_SYNCHRONIZATION_MESSAGE,
};
#[cfg(unix)]
use super::{proxy_control, serve_control, ControlResponse};
use mybrewfolio_sync_lib::{
    credentials::EncryptedFileCredentialStore, local::LocalError, model::SyncIssue, store::AppStore,
};

fn test_engine() -> (Arc<SyncEngine>, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let key = directory.path().join("key");
    std::fs::write(&key, [7_u8; 32]).expect("key written");
    let store = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).expect("store"));
    let credentials = Arc::new(
        EncryptedFileCredentialStore::from_key_file(directory.path().join("credentials.enc"), &key)
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
        json_file(Some(path.to_string_lossy().into_owned())).expect("JSON read")["restoreItemIds"]
            [0],
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
