use super::super::hash_value;
use super::{
    cloud_server, configure_test_cloud, connect_test_engine, gaggimate_server, sync_object,
    test_engine,
};
use crate::{local::GaggiMateClient, model::OAuthTokens};
use serde_json::json;

#[tokio::test]
async fn updates_status_counts_and_notes_metadata_from_cloud_state() {
    let (engine, _directory) = test_engine();
    engine
            .update_from_cloud_state(&json!({
                "source": {
                    "initialSyncConfiguredAt": "2026-08-20T10:00:00Z",
                    "duplicatePolicy": "import_all",
                    "notesSyncStatus": "two_way",
                    "notesSyncTargetDeviceId": "target",
                    "notesSyncWriterDeviceId": "writer",
                    "lastSyncAt": "2026-08-20T11:00:00Z"
                },
                "items": [
                    { "kind": "profile", "present": true },
                    { "kind": "shot", "present": true },
                    { "kind": "notes", "present": true },
                    { "kind": "shot", "present": false, "conflict": true },
                    { "kind": "shot", "suppressed": true }
                ],
                "noteBackups": [
                    { "id": "backup", "slot": "latest", "itemCount": 2, "createdAt": "2026-08-20T09:00:00Z", "finalizedAt": null },
                    { "slot": "invalid" }
                ]
            }))
            .await;

    let status = engine.status().await;
    assert_eq!(status.profiles, 1);
    assert_eq!(status.shots, 1);
    assert_eq!(status.notes, 1);
    assert_eq!(status.conflicts, 1);
    assert_eq!(status.suppressed, 1);
    assert!(status.initial_sync_configured);
    assert_eq!(status.duplicate_policy, "import_all");
    assert_eq!(status.notes_sync_status, "two_way");
    assert_eq!(status.note_backups.len(), 1);
    assert_eq!(status.last_sync_at.as_deref(), Some("2026-08-20T11:00:00Z"));
}

#[tokio::test]
async fn diagnose_reports_queue_counts_and_actionable_guidance() {
    let (engine, _directory) = test_engine();
    engine
        .store
        .set_setting("device_id", "device")
        .expect("device saved");
    engine
        .store
        .queue(&sync_object("pending", 10))
        .expect("pending queued");
    engine
        .store
        .record_failure(None, "shot", "failed", "read", "not available")
        .expect("failure recorded");

    let report = engine.diagnose().await.expect("diagnostics");

    assert_eq!(report["queue"]["pending"], 1);
    assert_eq!(report["queue"]["failures"], 1);
    assert_eq!(report["issues"][0]["sourceKey"], "failed");
    assert_eq!(report["issues"][0]["reason"], "not available");
    let codes = report["guidance"]
        .as_array()
        .expect("guidance list")
        .iter()
        .filter_map(|item| item["code"].as_str())
        .collect::<Vec<_>>();
    assert!(codes.contains(&"ACCOUNT_NOT_CONNECTED"));
    assert!(codes.contains(&"PENDING_UPLOADS"));
    assert!(codes.contains(&"SYNC_FAILURES"));
}

#[tokio::test]
async fn diagnose_reports_the_latest_local_error_without_sensitive_data() {
    let (engine, _directory) = test_engine();
    {
        let mut status = engine.status.write().await;
        status.connected = true;
        status.machine_reachable = true;
        status.last_error =
            Some("The GaggiMate returned invalid data while reading Notes for shot 123".into());
        status.last_error_code = Some("GAGGIMATE_DATA_INVALID".into());
        status.last_error_at = Some("2026-09-18T14:30:00Z".into());
    }

    let report = engine.diagnose().await.expect("diagnostics");

    assert_eq!(
        report["connection"]["lastError"],
        "The GaggiMate returned invalid data while reading Notes for shot 123"
    );
    assert_eq!(
        report["connection"]["lastErrorCode"],
        "GAGGIMATE_DATA_INVALID"
    );
    assert_eq!(report["connection"]["lastErrorAt"], "2026-09-18T14:30:00Z");
    assert!(report["connection"]["machineReachable"]
        .as_bool()
        .expect("machine reachability"));
}

#[tokio::test]
async fn host_and_local_preferences_round_trip_without_network_access() {
    let (engine, _directory) = test_engine();
    engine.set_host("127.0.0.1:8088").await.expect("host saved");
    assert_eq!(engine.status().await.machine_host, "127.0.0.1:8088");
    assert!(engine.set_host("https://public.example").await.is_err());
    assert!(!engine.hide_app_icon().expect("default icon setting"));
    engine.set_hide_app_icon(true).expect("icon hidden");
    assert!(engine.hide_app_icon().expect("icon setting"));
    engine
        .dismiss_notes_sync_intro()
        .await
        .expect("intro dismissed");
    assert!(engine.status().await.notes_sync_intro_seen);
}

#[tokio::test]
async fn sync_once_reads_gaggimate_queues_all_item_kinds_and_flushes_the_batch() {
    let (mut engine, _directory) = test_engine();
    let api_url = cloud_server(vec![
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z"},"items":[]}"#,
            r#"{"operations":[]}"#,
            "{}",
            "{}",
            "{}",
            "{}",
            r#"{"results":[{"index":0,"status":"created"},{"index":1,"status":"created"},{"index":2,"status":"created"}]}"#,
            "{}",
            r#"{"operations":[]}"#,
            "{}",
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z","duplicatePolicy":"reuse_matching"},"items":[{"kind":"profile"},{"kind":"shot"},{"kind":"notes"}]}"#,
        ])
        .await;
    configure_test_cloud(&mut engine, &api_url);
    connect_test_engine(&engine);
    let host = gaggimate_server().await;
    let local = GaggiMateClient::new(&host).expect("local client");
    let v7_shot = local.shot(1).await.expect("v7 shot reads");
    assert_eq!(v7_shot["samples"][0]["t"], 300.0);
    assert_eq!(v7_shot["samples"][0]["ct"], 92.5);
    assert_eq!(v7_shot["samples"][0]["wp"], 12.3);

    engine
        .store
        .set_setting("machine_host", &host)
        .expect("host saved");
    {
        let mut status = engine.status.write().await;
        status.last_error = Some("An earlier local error".into());
        status.last_error_code = Some("GAGGIMATE_DATA_INVALID".into());
        status.last_error_at = Some("2026-09-18T14:30:00Z".into());
    }

    let skipped = engine
        .queue_local_changes(&local, &json!({ "items": [] }))
        .await
        .expect("v7 shot queues");
    assert!(skipped.is_empty());
    let queued = engine.store.pending(25).expect("queue reads");
    let queued_shot = queued
        .iter()
        .find(|object| object.kind == "shot")
        .expect("shot is queued");
    assert_eq!(queued_shot.data["samples"][0]["wp"], 12.3);

    engine.sync_once().await.expect("sync succeeds");

    let status = engine.status().await;
    assert!(status.machine_reachable);
    assert!(status.last_sync_at.is_some());
    assert!(status.last_error.is_none());
    assert!(status.last_error_code.is_none());
    assert!(status.last_error_at.is_none());
    assert_eq!((status.profiles, status.shots, status.notes), (1, 1, 1));
    assert_eq!(engine.store.pending_count().expect("empty queue"), 0);
    assert_eq!(engine.store.failure_count().expect("no failures"), 0);
}

#[tokio::test]
async fn browser_and_device_pairing_register_the_same_connected_installation() {
    let (mut engine, _directory) = test_engine();
    let api_url = cloud_server(vec![
            r#"{"access_token":"browser-access","refresh_token":"browser-refresh","expires_in":3600}"#,
            r#"{"device":{"id":"desktop-device","sourceId":"desktop-source"}}"#,
            r#"{"requestId":"request-1","userCode":"ABCD-1234","verificationUri":"https://example.test/pair","pollToken":"poll-token","expiresIn":600}"#,
            r#"{"status":"authorized","authorizationCode":"device-code"}"#,
            r#"{"access_token":"device-access","refresh_token":"device-refresh","expires_in":3600}"#,
            r#"{"device":{"id":"headless-device","sourceId":"headless-source"}}"#,
        ])
        .await;
    configure_test_cloud(&mut engine, &api_url);

    let browser_url = engine.begin_oauth().await.expect("browser OAuth starts");
    let state = browser_url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .expect("OAuth state");
    engine
        .complete_oauth(&format!(
            "mybrewfolio-sync://oauth/callback?code=browser-code&state={state}"
        ))
        .await
        .expect("browser OAuth completes");
    assert_eq!(
        engine.status().await.this_device_id.as_deref(),
        Some("desktop-device")
    );

    let pairing = engine.begin_device_oauth().await.expect("pairing starts");
    assert_eq!(pairing.user_code, "ABCD-1234");
    assert!(engine.poll_device_oauth().await.expect("pairing completes"));
    assert_eq!(
        engine.status().await.this_device_id.as_deref(),
        Some("headless-device")
    );
    assert!(engine
        .credentials
        .pending_device_authorization()
        .expect("pairing state read")
        .is_none());
}

#[tokio::test]
async fn settings_and_resync_refresh_cloud_state_and_reset_stale_queue_data() {
    let (mut engine, _directory) = test_engine();
    let api_url = cloud_server(vec![
            "{}",
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z","duplicatePolicy":"import_all"},"items":[]}"#,
            r#"{"restored":1,"merged":0}"#,
            r#"{"source":{"initialSyncConfiguredAt":"2026-08-01T00:00:00Z"},"items":[]}"#,
        ])
        .await;
    configure_test_cloud(&mut engine, &api_url);
    connect_test_engine(&engine);
    engine
        .store
        .set_setting("shot_fingerprint_v2:1:2", "old")
        .expect("fingerprint saved");
    engine
        .store
        .queue(&sync_object("pending", 10))
        .expect("pending queued");

    engine.configure_sync(false).await.expect("settings saved");
    assert_eq!(engine.status().await.duplicate_policy, "import_all");
    let applied = engine
        .apply_resync(json!({ "restoreItemIds": ["restore-1"] }))
        .await
        .expect("resync applied");

    assert_eq!(applied["restored"], 1);
    assert_eq!(engine.store.pending_count().expect("queue reset"), 0);
    assert!(engine
        .store
        .setting("shot_fingerprint_v2:1:2")
        .expect("fingerprint read")
        .is_none());
}

#[tokio::test]
async fn notes_backup_and_resync_preview_use_the_current_local_inventory() {
    let (mut engine, _directory) = test_engine();
    let api_url = cloud_server(vec![
        r#"{"backup":{"id":"backup-1"}}"#,
        "{}",
        "{}",
        r#"{"source":{},"items":[]}"#,
        r#"{"restoreItems":[{"kind":"shot","sourceKey":"1:1735689600"}],"duplicates":[]}"#,
    ])
    .await;
    configure_test_cloud(&mut engine, &api_url);
    connect_test_engine(&engine);
    engine
        .store
        .set_setting("machine_host", &gaggimate_server().await)
        .expect("host saved");

    let backup_id = engine
        .create_latest_notes_backup()
        .await
        .expect("backup created");
    let preview = engine.resync_preview().await.expect("resync preview");

    assert_eq!(backup_id, "backup-1");
    assert_eq!(preview["restoreItems"][0]["sourceKey"], "1:1735689600");
}

#[cfg(feature = "headless")]
#[tokio::test]
async fn headless_activation_uses_fresh_status_and_never_takes_over_another_writer() {
    for (mode, device, expected) in [
        ("two_way", "device-1", "already"),
        ("two_way", "another-device", "another installation"),
        (
            "activation_pending",
            "another-device",
            "another installation",
        ),
        ("unknown", "device-1", "valid Notes Sync status"),
    ] {
        let (mut engine, _directory) = test_engine();
        connect_test_engine(&engine);
        engine.status.write().await.connected = true;
        let body = json!({"source": {"notes_sync_status": mode,
                "notes_sync_writer_device_id": device, "notes_sync_target_device_id": device}})
        .to_string();
        // Exactly one read is served. No request/backup endpoints are available.
        let api_url = cloud_server(vec![&body]).await;
        configure_test_cloud(&mut engine, &api_url);
        assert_eq!(engine.status().await.notes_sync_status, "one_way");
        let result = engine.prepare_headless_notes_activation().await;
        if expected == "already" {
            assert_eq!(result.unwrap(), json!({"alreadyEnabled": true}));
        } else {
            assert!(result.unwrap_err().contains(expected));
        }
    }
    let (engine, _directory) = test_engine();
    assert!(engine
        .prepare_headless_notes_activation()
        .await
        .unwrap_err()
        .contains("Connect this installation"));
}

#[cfg(feature = "headless")]
#[tokio::test]
async fn headless_activation_prepares_backup_and_confirms_only_its_pending_assignment() {
    let (mut engine, _directory) = test_engine();
    connect_test_engine(&engine);
    engine.status.write().await.connected = true;
    engine
        .store
        .set_setting("machine_host", &gaggimate_server().await)
        .unwrap();
    let api_url = cloud_server(vec![
            r#"{"source":{"notes_sync_status":"one_way"}}"#,
            "{}", r#"{"backup":{"id":"activation-backup"}}"#, "{}", "{}",
            r#"{"items":[]}"#,
            r#"{"source":{"notes_sync_status":"activation_pending","notes_sync_target_device_id":"device-1"}}"#,
            r#"{"status":"two_way"}"#,
            r#"{"source":{"notes_sync_status":"two_way","notes_sync_writer_device_id":"device-1"}}"#,
        ]).await;
    configure_test_cloud(&mut engine, &api_url);
    let preview = engine.prepare_headless_notes_activation().await.unwrap();
    assert_eq!(preview["backupId"], "activation-backup");
    assert_eq!(
        engine
            .activate_headless_notes("activation-backup", json!([]))
            .await
            .unwrap()["status"],
        "two_way"
    );

    let api_url = cloud_server(vec![r#"{"source":{"notes_sync_status":"activation_pending","notes_sync_target_device_id":"another-device"}}"#]).await;
    configure_test_cloud(&mut engine, &api_url);
    assert!(engine
        .activate_headless_notes("activation-backup", json!([]))
        .await
        .unwrap_err()
        .contains("no longer assigned"));
}

#[tokio::test]
async fn notes_activation_and_restore_preserve_the_verified_machine_content() {
    let (mut engine, _directory) = test_engine();
    let source_key = "1:1735689600";
    let notes = json!({ "text": "dial in finer" });
    let notes_hash = hash_value(&notes);
    let api_url = cloud_server(vec![
        "{}",
        r#"{"backup":{"id":"activation-backup"}}"#,
        "{}",
        "{}",
        r#"{"items":[{"sourceKey":"1:1735689600","differs":true}]}"#,
    ])
    .await;
    configure_test_cloud(&mut engine, &api_url);
    connect_test_engine(&engine);
    engine
        .store
        .set_setting("machine_host", &gaggimate_server().await)
        .expect("host saved");

    let activation = engine
        .begin_two_way_notes_activation()
        .await
        .expect("activation preview");

    assert_eq!(activation["backupId"], "activation-backup");
    assert!(engine.status().await.notes_sync_intro_seen);

    let (mut restore_engine, _directory) = test_engine();
    let restore_api = cloud_server(vec![
            r#"{"backup":{"id":"latest-backup"}}"#,
            "{}",
            "{}",
            &format!(
                r#"{{"items":[{{"sourceKey":"{source_key}","notes":{notes},"notesHash":"{notes_hash}"}}]}}"#
            ),
            r#"{"applied":1}"#,
        ])
        .await;
    configure_test_cloud(&mut restore_engine, &restore_api);
    connect_test_engine(&restore_engine);
    restore_engine
        .store
        .set_setting("machine_host", &gaggimate_server().await)
        .expect("host saved");

    let restored = restore_engine
        .restore_notes_backup("selected-backup", &[source_key.into()])
        .await
        .expect("notes restored");

    assert_eq!(restored, json!({ "applied": 1, "skipped": 0 }));
}

#[tokio::test]
async fn outbound_notes_report_conflicts_and_successful_machine_writes() {
    let (mut engine, _directory) = test_engine();
    let current_notes = json!({ "text": "dial in finer" });
    let api_url = cloud_server(vec![
            &format!(
                r#"{{"operations":[
                    {{"id":"conflict","leaseToken":"lease-1","sourceKey":"1:1735689600","baseSourceHash":"not-current","desiredNotes":{{}}}},
                    {{"id":"apply","leaseToken":"lease-2","sourceKey":"1:1735689600","baseSourceHash":"{}","desiredNotes":{{"text":"new note"}}}}
                ]}}"#,
                hash_value(&current_notes)
            ),
            "{}",
            "{}",
        ])
        .await;
    configure_test_cloud(&mut engine, &api_url);
    connect_test_engine(&engine);
    let local = GaggiMateClient::new(&gaggimate_server().await).expect("local client");

    engine
        .process_outbound_notes(&local, "device-1")
        .await
        .expect("outbound operations processed");
    let issues = engine.store.failures().expect("write issues");
    assert!(issues.iter().any(|issue| {
        issue.kind == "notes"
            && issue.stage == "write"
            && issue.reason
                == "GaggiMate did not confirm the Notes update. Sync will retry automatically."
    }));
}

#[tokio::test]
async fn disconnect_always_clears_local_account_data_when_the_server_is_unavailable() {
    let (mut engine, _directory) = test_engine();
    engine.cloud.config.api_url = "http://127.0.0.1:1".into();
    engine
        .store
        .set_setting("device_id", "device-1")
        .expect("device saved");
    engine
        .store
        .set_setting("source_id", "source-1")
        .expect("source saved");
    engine
        .store
        .queue(&sync_object("pending", 10))
        .expect("pending queued");
    engine
        .credentials
        .save_tokens(&OAuthTokens {
            access_token: "valid-access".into(),
            refresh_token: None,
            expires_at: i64::MAX,
        })
        .expect("token saved");

    let result = engine.disconnect().await.expect("disconnect succeeds");

    assert_eq!(result["serverRevoked"], false);
    assert_eq!(result["credentialsRemoved"], true);
    assert!(!engine.status().await.connected);
    assert!(engine
        .store
        .setting("device_id")
        .expect("device read")
        .is_none());
    assert_eq!(engine.store.pending_count().expect("queue read"), 0);
    assert!(engine.credentials.tokens().expect("tokens read").is_none());
}
