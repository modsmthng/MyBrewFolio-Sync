use std::sync::{Arc, Mutex as StdMutex};

use super::{
    api_timestamp, batch_result_status, diagnostic_guidance, hash_value, is_terminal_batch_status,
    normalized_notes, notes_are_semantically_empty, profile_store_local_error,
    profile_store_public_error_message, profiles_equal, scan_due, select_sync_batch,
    serialized_batch_bytes, shot_fingerprint, shot_read_failure, shot_source_key,
    should_refresh_notes, suppressed_items, sync_progress_log_line, EngineError, NotesWriteOutcome,
    SyncEngine, MAX_SYNC_BATCH_BYTES, NOTES_WRITE_ATTEMPTS,
};
use crate::model::{
    AppStatus, IndexEntry, OAuthTokens, SyncObject, SyncProgress, SyncProgressPhase,
};
use crate::{
    cloud::CloudConfig,
    credentials::EncryptedFileCredentialStore,
    local::{GaggiMateClient, LocalError},
    store::AppStore,
};
use chrono::{Duration, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Clone)]
struct NotesFixtureState {
    current: Value,
    pending: Option<Value>,
    stale_reads_remaining: usize,
    ignore_writes: bool,
    external_change_after_first_write: Option<Value>,
    writes: usize,
}

async fn notes_write_server(
    initial: Value,
    stale_reads: usize,
    ignore_writes: bool,
    external_change_after_first_write: Option<Value>,
) -> (String, Arc<StdMutex<NotesFixtureState>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("GaggiMate Notes listener");
    let address = listener.local_addr().expect("GaggiMate Notes address");
    let state = Arc::new(StdMutex::new(NotesFixtureState {
        current: initial,
        pending: None,
        stale_reads_remaining: stale_reads,
        ignore_writes,
        external_change_after_first_write,
        writes: 0,
    }));
    let server_state = state.clone();
    tokio::spawn(async move {
        for _ in 0..16 {
            let (stream, _) = listener.accept().await.expect("GaggiMate Notes request");
            let mut socket = accept_async(stream)
                .await
                .expect("GaggiMate Notes WebSocket");
            let request = socket
                .next()
                .await
                .expect("GaggiMate Notes message")
                .expect("valid GaggiMate Notes message")
                .into_text()
                .expect("text GaggiMate Notes message");
            let request: Value = serde_json::from_str(&request).expect("GaggiMate Notes JSON");
            let rid = request["rid"].as_str().expect("GaggiMate Notes request ID");
            let response = match request["tp"].as_str() {
                Some("req:history:notes:get") => {
                    let mut state = server_state.lock().expect("Notes state");
                    if state.pending.is_some() && state.stale_reads_remaining == 0 {
                        state.current = state.pending.take().expect("pending Notes");
                    } else if state.pending.is_some() {
                        state.stale_reads_remaining -= 1;
                    }
                    json!({
                        "tp": "res:history:notes:get", "rid": rid,
                        "notes": state.current,
                    })
                }
                Some("req:history:notes:save") => {
                    let mut state = server_state.lock().expect("Notes state");
                    state.writes += 1;
                    if state.writes == 1 {
                        if let Some(external) = state.external_change_after_first_write.take() {
                            state.current = external;
                            state.pending = None;
                        } else if !state.ignore_writes {
                            state.pending = Some(request["notes"].clone());
                        }
                    } else if !state.ignore_writes {
                        state.pending = Some(request["notes"].clone());
                    }
                    json!({ "tp": "res:history:notes:save", "rid": rid })
                }
                _ => json!({ "tp": "res:error", "rid": rid, "error": "unexpected request" }),
            };
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .expect("GaggiMate Notes response");
        }
    });
    (format!("127.0.0.1:{}", address.port()), state)
}

pub(super) fn status() -> AppStatus {
    AppStatus {
        connected: true,
        machine_host: "gaggimate.local".into(),
        machine_reachable: true,
        syncing: false,
        last_sync_at: None,
        last_error: None,
        last_error_code: None,
        last_error_at: None,
        sync_progress: None,
        profiles: 0,
        shots: 10,
        notes: 0,
        conflicts: 0,
        suppressed: 350,
        initial_sync_configured: true,
        duplicate_policy: "reuse_matching".into(),
        notes_sync_status: "one_way".into(),
        notes_sync_target_device_id: None,
        notes_sync_writer_device_id: None,
        this_device_id: None,
        notes_sync_intro_seen: false,
        note_backups: Vec::new(),
        issues: Vec::new(),
    }
}

#[test]
fn hashes_json_like_the_sync_api() {
    let value = serde_json::json!({
        "b": 1,
        "a": [true, { "z": null, "x": "café" }],
        "n": 1.25
    });
    assert_eq!(
        hash_value(&value),
        "383410d6c75c6bd29378b3b9da39e37fde1ab284f8f1cb0230ecc2c196f5d346"
    );
}

#[test]
fn recognizes_absent_and_default_gaggimate_notes_as_empty() {
    assert!(notes_are_semantically_empty(&json!({})));
    assert!(notes_are_semantically_empty(&json!({
        "id": "1",
        "timestamp": 1_735_689_600,
        "rating": 0,
        "beanType": "",
        "doseIn": "",
        "doseOut": "",
        "ratio": "",
        "grindSetting": "",
        "balanceTaste": "balanced",
        "notes": "",
    })));
    assert!(!notes_are_semantically_empty(
        &json!({ "notes": "Keep this" })
    ));
    assert_eq!(
        normalized_notes(json!({ "id": "1", "rating": 0 })),
        json!({})
    );
}

#[tokio::test]
async fn leaves_empty_notes_out_of_one_way_sync() {
    let (engine, _directory) = test_engine();
    let host = gaggimate_server_with_notes(json!({})).await;
    let local = GaggiMateClient::new(&host).expect("local client");

    engine
        .queue_local_changes(
            &local,
            &json!({ "source": { "notesSyncStatus": "one_way" }, "items": [] }),
        )
        .await
        .expect("one-way scan succeeds");

    assert!(engine
        .store
        .pending(25)
        .expect("pending objects")
        .iter()
        .all(|object| object.kind != "notes"));
}

#[tokio::test]
async fn queues_empty_notes_once_for_the_two_way_protocol_baseline() {
    let (engine, _directory) = test_engine();
    let host = gaggimate_server_with_notes(json!({})).await;
    let local = GaggiMateClient::new(&host).expect("local client");
    let state = json!({ "source": { "notesSyncStatus": "two_way" }, "items": [] });
    let source_key = "1:1735689600";

    engine
        .queue_local_changes(&local, &state)
        .await
        .expect("two-way baseline scan succeeds");
    let queued = engine.store.pending(25).expect("pending objects");
    let note = queued
        .iter()
        .find(|object| object.kind == "notes")
        .expect("empty Note baseline is queued");
    assert_eq!(note.source_key, source_key);
    assert_eq!(note.data, json!({}));
    assert_eq!(
        engine
            .store
            .setting("notes_reader_version")
            .expect("reader version"),
        Some("4".into())
    );

    engine
        .store
        .remove_pending("notes", source_key)
        .expect("baseline removed for second-scan assertion");
    engine
        .queue_local_changes(&local, &state)
        .await
        .expect("second scan succeeds");
    assert!(engine
        .store
        .pending(25)
        .expect("pending objects")
        .iter()
        .all(|object| object.kind != "notes"));
}

#[tokio::test]
async fn retries_gaggimate_notes_writes_until_the_target_is_verified() {
    let (engine, _directory) = test_engine();
    let base = json!({ "notes": "before" });
    let desired = json!({ "notes": "after" });
    let (host, state) = notes_write_server(base.clone(), 2, false, None).await;
    let local = GaggiMateClient::new(&host).expect("local client");

    let result = engine
        .write_and_verify_notes(&local, 1, &hash_value(&base), &desired)
        .await
        .expect("write verification succeeds");

    match result {
        NotesWriteOutcome::Applied(actual) => assert_eq!(actual, desired),
        _ => panic!("the delayed write should be verified"),
    }
    assert_eq!(state.lock().expect("Notes state").writes, 3);
}

#[tokio::test]
async fn verifies_restoring_empty_notes_against_firmware_defaults() {
    let (engine, _directory) = test_engine();
    let defaults = json!({"id":"1", "rating":0, "notes":"", "balanceTaste":"balanced"});
    let (host, _state) = notes_write_server(defaults, 0, true, None).await;
    let local = GaggiMateClient::new(&host).unwrap();
    let result = engine
        .write_and_verify_notes(&local, 1, &hash_value(&json!({})), &json!({}))
        .await
        .unwrap();
    assert!(matches!(result, NotesWriteOutcome::Applied(actual) if actual == json!({})));
}

#[tokio::test]
async fn reports_an_unverified_gaggimate_notes_write_without_false_success() {
    let (engine, _directory) = test_engine();
    let base = json!({ "notes": "before" });
    let desired = json!({ "notes": "after" });
    let (host, state) = notes_write_server(base.clone(), 0, true, None).await;
    let local = GaggiMateClient::new(&host).expect("local client");

    let result = engine
        .write_and_verify_notes(&local, 1, &hash_value(&base), &desired)
        .await
        .expect("unverified write is a handled outcome");

    assert!(matches!(result, NotesWriteOutcome::Unverified));
    assert_eq!(
        state.lock().expect("Notes state").writes,
        NOTES_WRITE_ATTEMPTS
    );
}

#[tokio::test]
async fn stops_retrying_when_notes_change_externally_during_verification() {
    let (engine, _directory) = test_engine();
    let base = json!({ "notes": "before" });
    let desired = json!({ "notes": "from MyBrewFolio" });
    let external = json!({ "notes": "edited on GaggiMate" });
    let (host, state) = notes_write_server(base.clone(), 0, false, Some(external.clone())).await;
    let local = GaggiMateClient::new(&host).expect("local client");

    let result = engine
        .write_and_verify_notes(&local, 1, &hash_value(&base), &desired)
        .await
        .expect("external edit is a handled outcome");

    match result {
        NotesWriteOutcome::Conflict(actual) => assert_eq!(actual, external),
        _ => panic!("the external edit must become a conflict"),
    }
    assert_eq!(state.lock().expect("Notes state").writes, 1);
}

#[tokio::test]
async fn completes_an_outbound_retry_when_the_machine_already_has_the_target_notes() {
    let (mut engine, _directory) = test_engine();
    let current = json!({ "notes": "already written" });
    let api_url = cloud_server(vec![
            &format!(
                r#"{{"status":"ready","operations":[{{"id":"retry","leaseToken":"lease-1","sourceKey":"1:1735689600","baseSourceHash":"{}","desiredNotes":{current}}}]}}"#,
                hash_value(&json!({ "notes": "before" })),
            ),
            "{}",
        ])
        .await;
    configure_test_cloud(&mut engine, &api_url);
    connect_test_engine(&engine);
    let (host, state) = notes_write_server(current, 0, false, None).await;
    let local = GaggiMateClient::new(&host).expect("local client");

    engine
        .process_outbound_notes(&local, "device-1")
        .await
        .expect("already-applied retry completes");

    assert_eq!(state.lock().expect("Notes state").writes, 0);
    assert!(engine.store.failures().expect("write issues").is_empty());
}

#[tokio::test]
async fn keeps_a_legacy_backup_requirement_manual() {
    let (mut engine, _directory) = test_engine();
    let api_url = cloud_server(vec![r#"{"status":"backup_required","operations":[]}"#]).await;
    configure_test_cloud(&mut engine, &api_url);
    connect_test_engine(&engine);
    let local = GaggiMateClient::new("127.0.0.1:1").expect("local client");

    engine
        .process_outbound_notes(&local, "device-1")
        .await
        .expect("manual backup requirement is non-blocking");

    assert!(engine
        .store
        .failures()
        .expect("backup issue")
        .iter()
        .any(|issue| {
            issue.kind == "notes"
                && issue.source_key == "outbound"
                && issue.stage == "backup"
                && issue.reason == "A manual Latest Backup is required before Notes can be updated."
        }));
}

#[test]
fn profile_comparison_ignores_only_machine_runtime_selection_state() {
    let installed = json!({
        "id": "profile-1",
        "label": "Flat white",
        "favorite": true,
        "selected": false,
        "phases": [{ "name": "Preinfusion", "temperature": 93 }]
    });
    let store = json!({
        "id": "profile-1",
        "label": "Flat white",
        "favorite": false,
        "selected": true,
        "phases": [{ "name": "Preinfusion", "temperature": 93 }]
    });
    let changed = json!({
        "id": "profile-1",
        "label": "Flat white",
        "phases": [{ "name": "Preinfusion", "temperature": 94 }]
    });

    assert!(profiles_equal(&installed, &store));
    assert!(!profiles_equal(&installed, &changed));
}

#[test]
fn normalizes_integer_shaped_numbers() {
    let value = serde_json::json!({
        "samples": [0, 1.0, 1.25, -2.5],
        "name": "Shot"
    });
    assert_eq!(
        hash_value(&value),
        "8022fd9de6812be583b599abbf16921a856d33314b7c0db32dba8b9d515b0f3f"
    );
}

#[test]
fn unsupported_shot_format_has_an_actionable_failure_message() {
    assert_eq!(
            shot_read_failure(&LocalError::UnsupportedShotFormat(7)),
            "GaggiMate shot format v7 is not supported by this MyBrewFolio Sync version. Update MyBrewFolio Sync before retrying this shot."
        );
    assert_eq!(
        shot_read_failure(&LocalError::InvalidShot(123)),
        "The GaggiMate returned invalid data while reading shot 123"
    );
}

#[test]
fn formats_api_timestamps_as_utc_zulu_time() {
    let timestamp = Utc
        .with_ymd_and_hms(2026, 7, 27, 9, 41, 33)
        .single()
        .expect("valid timestamp");
    assert_eq!(api_timestamp(timestamp), "2026-07-27T09:41:33.000Z");
}

#[tokio::test]
async fn keeps_first_sync_progress_visible_until_a_terminal_heartbeat() {
    let (engine, _directory) = test_engine();
    let progress = SyncProgress {
        phase: SyncProgressPhase::ReadingHistory,
        scanned_shots: 8,
        total_shots: 20,
        uploaded_items: None,
        total_items: None,
    };

    engine
        .report_sync_progress("device-1", progress.clone(), false)
        .await
        .expect("local progress is stored");
    assert_eq!(engine.status().await.sync_progress, Some(progress));
    assert_eq!(
        engine.diagnose().await.expect("diagnostics")["connection"]["syncProgress"]["phase"],
        "reading_history"
    );

    engine.clear_sync_progress().await;
    assert_eq!(engine.status().await.sync_progress, None);
}

#[test]
fn formats_first_sync_progress_for_container_logs() {
    assert_eq!(
        sync_progress_log_line(&SyncProgress {
            phase: SyncProgressPhase::Uploading,
            scanned_shots: 20,
            total_shots: 20,
            uploaded_items: Some(25),
            total_items: Some(40),
        }),
        "First sync: Uploading: 25 of 40 items"
    );
}

#[test]
fn diagnostics_explain_safely_reused_matches() {
    let guidance = diagnostic_guidance(&status(), 0, 0);
    assert_eq!(guidance[0]["code"], "MATCHING_ITEMS_REUSED");
    assert_eq!(guidance[0]["nextCommand"], "resync preview");
    assert!(guidance[0]["message"]
        .as_str()
        .expect("diagnostic message")
        .contains("Nothing was restored or imported automatically"));
}

#[test]
fn diagnostics_keep_invalid_data_context_and_do_not_treat_it_as_unreachable() {
    let mut status = status();
    status.last_error =
        Some("The GaggiMate returned invalid data while reading Notes for shot 123".into());
    status.last_error_code = Some("GAGGIMATE_DATA_INVALID".into());
    status.last_error_at = Some("2026-09-18T14:30:00Z".into());
    status.machine_reachable = true;

    let report = diagnostic_guidance(&status, 0, 0);
    assert_eq!(report[0]["code"], "GAGGIMATE_DATA_INVALID");
    assert_eq!(
        report[0]["message"],
        "The GaggiMate returned invalid data while reading Notes for shot 123"
    );
    assert!(EngineError::Local(LocalError::InvalidNotes(123)).machine_reachable());
}

#[test]
fn diagnostics_explain_docker_local_hostname_recovery_without_exposing_an_ip() {
    let mut status = status();
    status.last_error_code = Some("GAGGIMATE_UNREACHABLE".into());
    status.last_error_at = Some("2026-09-18T14:30:00Z".into());
    status.machine_reachable = false;

    let report = diagnostic_guidance(&status, 0, 0);
    let message = report[0]["message"].as_str().expect("message");
    assert!(message.contains("MYBREWFOLIO_SYNC_GAGGIMATE_HOST"));
    assert!(message.contains("private LAN IP"));
    assert!(!message.contains("192.168."));
}

#[test]
fn profile_store_errors_keep_local_profile_ids_out_of_remote_messages() {
    let local_message = "The GaggiMate returned invalid data while updating profile profile-abc";
    let public_message = profile_store_public_error_message("PROFILE_SAVE_FAILED", local_message);
    assert_eq!(
            public_message,
            "GaggiMate could not complete this profile operation. Check local Sync diagnostics and try again."
        );
    assert!(!public_message.contains("profile-abc"));

    let (code, message) =
        profile_store_local_error(LocalError::InvalidProfileUpdate("profile-abc".into()));
    assert_eq!(code, "GAGGIMATE_DATA_INVALID");
    assert_eq!(message, local_message);
}

pub(super) fn sync_object(key: &str, bytes: usize) -> SyncObject {
    SyncObject {
        kind: "shot".into(),
        source_key: key.into(),
        source_hash: "a".repeat(64),
        shot_source_key: None,
        data: json!({ "payload": serde_json::Value::String("x".repeat(bytes)) }),
    }
}

#[test]
fn splits_batches_before_the_api_limit() {
    let pending = vec![
        sync_object("one", 3_800_000),
        sync_object("two", 3_800_000),
        sync_object("three", 100),
    ];

    let (batch, oversized) = select_sync_batch(&pending);

    assert!(oversized.is_none());
    assert_eq!(batch.len(), 1);
    assert!(serialized_batch_bytes(&batch) <= MAX_SYNC_BATCH_BYTES);
}

#[test]
fn identifies_an_object_that_cannot_fit_in_any_batch() {
    let pending = vec![sync_object("too-large", MAX_SYNC_BATCH_BYTES)];

    let (batch, oversized) = select_sync_batch(&pending);

    assert!(batch.is_empty());
    assert_eq!(oversized.expect("oversized object").source_key, "too-large");
}

#[test]
fn identifies_suppressed_items_in_both_json_naming_styles() {
    let suppressed = suppressed_items(&json!({ "items": [
        { "kind": "shot", "sourceKey": "1:100", "suppressed": true },
        { "kind": "notes", "source_key": "1:100", "suppressed": true },
        { "kind": "profile", "sourceKey": "p1", "suppressed": false },
        { "kind": "shot", "sourceKey": "missing" }
    ]}));

    assert!(suppressed.contains(&("shot".into(), "1:100".into())));
    assert!(suppressed.contains(&("notes".into(), "1:100".into())));
    assert_eq!(suppressed.len(), 2);
}

#[test]
fn scan_and_note_refresh_decisions_respect_intervals_and_recent_window() {
    let now = Utc::now();
    assert!(scan_due(now, None, Duration::minutes(5)));
    assert!(scan_due(
        now,
        Some(now - Duration::minutes(6)),
        Duration::minutes(5)
    ));
    assert!(!scan_due(
        now,
        Some(now - Duration::minutes(4)),
        Duration::minutes(5)
    ));
    assert!(should_refresh_notes(false, false, true, 19));
    assert!(!should_refresh_notes(false, false, true, 20));
    assert!(should_refresh_notes(true, false, false, 99));
    assert!(should_refresh_notes(false, true, false, 99));
}

#[test]
fn shot_identity_and_batch_result_helpers_have_stable_defaults() {
    let entry = IndexEntry {
        id: 7,
        timestamp: 123,
        duration: 456,
        volume: Some(18.5),
        rating: Some(4),
        profile_id: "profile".into(),
        profile_name: "Espresso".into(),
        incomplete: false,
    };
    assert_eq!(shot_source_key(&entry), "7:123");
    assert_eq!(shot_fingerprint(&entry), "123:456:18.5:4");
    assert_eq!(
        batch_result_status(&json!({ "index": 2, "status": "created" })),
        (2, "created")
    );
    assert_eq!(batch_result_status(&json!({})), (usize::MAX, "invalid"));
    assert!(is_terminal_batch_status("suppressed"));
    assert!(!is_terminal_batch_status("retry"));
}

#[tokio::test]
async fn sync_schedule_defaults_to_thirty_seconds_and_applies_valid_cloud_changes() {
    let (engine, _directory) = test_engine();
    assert_eq!(engine.sync_interval_seconds(), 30);

    engine
        .set_sync_interval_seconds(1200)
        .expect("save local schedule");
    assert_eq!(engine.sync_interval_seconds(), 1200);

    let restarted = SyncEngine::open(engine.store.clone(), engine.credentials.clone())
        .expect("reopen engine with persisted schedule");
    assert_eq!(restarted.sync_interval_seconds(), 1200);

    restarted
        .apply_sync_interval_from_state(&json!({
            "source": { "sync_interval_seconds": 60 }
        }))
        .expect("apply server schedule");
    assert_eq!(restarted.sync_interval_seconds(), 60);

    restarted
        .apply_sync_interval_from_state(&json!({
            "source": { "sync_interval_seconds": 90 }
        }))
        .expect("ignore unsupported server schedule");
    assert_eq!(restarted.sync_interval_seconds(), 60);
}

pub(super) fn test_engine() -> (SyncEngine, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let key_path = directory.path().join("key");
    std::fs::write(&key_path, [3_u8; 32]).expect("key written");
    let credentials = Arc::new(
        EncryptedFileCredentialStore::from_key_file(
            directory.path().join("credentials.enc"),
            &key_path,
        )
        .expect("credentials store"),
    );
    let store = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).expect("store"));
    (
        SyncEngine::open(store, credentials).expect("engine"),
        directory,
    )
}

pub(super) async fn cloud_server(responses: Vec<&str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("cloud listener");
    let address = listener.local_addr().expect("cloud address");
    let responses = responses
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    tokio::spawn(async move {
        for body in responses {
            let (mut stream, _) = listener.accept().await.expect("cloud request");
            let mut request = [0_u8; 8_192];
            let _ = stream.read(&mut request).await.expect("cloud request read");
            let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("cloud response");
        }
    });
    format!("http://{address}")
}

pub(super) fn history_index() -> Vec<u8> {
    let mut bytes = vec![0_u8; 32 + 128];
    bytes[0..4].copy_from_slice(&0x5844_4953_u32.to_le_bytes());
    bytes[6..8].copy_from_slice(&128_u16.to_le_bytes());
    bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
    let entry = 32;
    bytes[entry..entry + 4].copy_from_slice(&1_u32.to_le_bytes());
    bytes[entry + 4..entry + 8].copy_from_slice(&1_735_689_600_u32.to_le_bytes());
    bytes[entry + 8..entry + 12].copy_from_slice(&300_u32.to_le_bytes());
    bytes[entry + 12..entry + 14].copy_from_slice(&180_u16.to_le_bytes());
    bytes[entry + 14] = 4;
    bytes[entry + 15] = 1;
    bytes[entry + 16..entry + 25].copy_from_slice(b"profile-1");
    bytes[entry + 48..entry + 60].copy_from_slice(b"Test profile");
    bytes
}

pub(super) fn history_shot() -> Vec<u8> {
    let mut bytes = vec![0_u8; 512 + 30];
    bytes[0..4].copy_from_slice(&0x544f_4853_u32.to_le_bytes());
    bytes[4] = 7;
    bytes[5] = 30;
    bytes[6..8].copy_from_slice(&512_u16.to_le_bytes());
    bytes[8..10].copy_from_slice(&250_u16.to_le_bytes());
    bytes[12..16].copy_from_slice(&0x3fff_u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
    bytes[20..24].copy_from_slice(&300_u32.to_le_bytes());
    bytes[24..28].copy_from_slice(&1_735_689_600_u32.to_le_bytes());
    bytes[28..37].copy_from_slice(b"profile-1");
    bytes[60..72].copy_from_slice(b"Test profile");
    bytes[108..110].copy_from_slice(&180_u16.to_le_bytes());
    bytes[512..516].copy_from_slice(&300_u32.to_le_bytes());
    let values = [
        930_u16, 925, 20, 18, 180, 200, 170, 0, 180, 180, 0, 0x000d, 123,
    ];
    for (index, value) in values.iter().enumerate() {
        let offset = 516 + index * 2;
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub(super) async fn gaggimate_server() -> String {
    gaggimate_server_with_notes(json!({ "text": "dial in finer" })).await
}

async fn gaggimate_server_with_notes(notes: Value) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("GaggiMate listener");
    let address = listener.local_addr().expect("GaggiMate address");
    tokio::spawn(async move {
        for _ in 0..8 {
            let (mut stream, _) = listener.accept().await.expect("GaggiMate request");
            let mut prefix = [0_u8; 64];
            let length = stream.peek(&mut prefix).await.expect("request preview");
            if String::from_utf8_lossy(&prefix[..length]).starts_with("GET /ws") {
                let mut socket = accept_async(stream).await.expect("WebSocket accepted");
                let request = socket
                    .next()
                    .await
                    .expect("WebSocket request")
                    .expect("valid WebSocket message")
                    .into_text()
                    .expect("text WebSocket message");
                let request: serde_json::Value =
                    serde_json::from_str(&request).expect("JSON WebSocket request");
                let rid = request["rid"].as_str().expect("request ID");
                let response = match request["tp"].as_str() {
                    Some("req:profiles:list") => json!({
                        "tp": "res:profiles:list", "rid": rid,
                        "profiles": [{ "id": "profile-1" }]
                    }),
                    Some("req:profiles:load") => json!({
                        "tp": "res:profiles:load", "rid": rid,
                        "profile": { "id": "profile-1", "name": "Test profile" }
                    }),
                    Some("req:history:notes:get") => json!({
                        "tp": "res:history:notes:get", "rid": rid,
                        "notes": notes.clone()
                    }),
                    Some("req:history:notes:save") => json!({
                        "tp": "res:history:notes:save", "rid": rid
                    }),
                    _ => {
                        json!({ "tp": "res:error", "rid": rid, "error": "unexpected request" })
                    }
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .expect("WebSocket response");
            } else {
                let mut request = [0_u8; 1_024];
                let length = stream.read(&mut request).await.expect("HTTP request read");
                let request = String::from_utf8_lossy(&request[..length]);
                let (content_type, body) = if request.starts_with("GET /api/history/index.bin") {
                    ("application/octet-stream", history_index())
                } else if request.starts_with("GET /api/history/000001.slog") {
                    ("application/octet-stream", history_shot())
                } else {
                    ("application/json", b"{}".to_vec())
                };
                let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("HTTP headers");
                stream.write_all(&body).await.expect("HTTP body");
            }
        }
    });
    format!("127.0.0.1:{}", address.port())
}

pub(super) fn configure_test_cloud(engine: &mut SyncEngine, api_url: &str) {
    engine.cloud.config = CloudConfig {
        api_url: api_url.into(),
        client_id: "test-client".into(),
        authorize_url: format!("{api_url}/authorize"),
        token_url: format!("{api_url}/token"),
        redirect_uri: "mybrewfolio-sync://oauth/callback".into(),
        device_redirect_uri: format!("{api_url}/callback"),
    };
}

pub(super) fn connect_test_engine(engine: &SyncEngine) {
    engine
        .credentials
        .save_tokens(&OAuthTokens {
            access_token: "valid-access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        })
        .expect("token saved");
    engine
        .store
        .set_setting("device_id", "device-1")
        .expect("device saved");
}

mod integration;
