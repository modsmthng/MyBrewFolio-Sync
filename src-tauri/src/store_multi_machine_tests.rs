// SPDX-License-Identifier: GPL-3.0-or-later

use super::{AppStore, MachineRecord};
use crate::model::SyncObject;
use rusqlite::{params, Connection};
use serde_json::json;

fn pending_shot(source_key: &str, name: &str) -> SyncObject {
    SyncObject {
        kind: "shot".into(),
        source_key: source_key.into(),
        source_hash: format!("hash-{name}"),
        shot_source_key: None,
        data: json!({ "name": name }),
    }
}

fn seed_single_machine_database(path: &std::path::Path) {
    // This is the schema that v0.5.6 wrote before the machine registry existed.
    let connection = Connection::open(path).expect("legacy database opens");
    connection
        .execute_batch(
            "create table settings (key text primary key, value text not null);
             create table pending_objects (
               kind text not null, source_key text not null, source_hash text not null,
               payload text not null, shot_source_key text, updated_at integer not null,
               primary key (kind, source_key)
             );",
        )
        .expect("legacy schema created");
    for (key, value) in [
        ("source_id", "11111111-1111-4111-8111-111111111111"),
        ("device_id", "legacy-device"),
        ("machine_host", "office.local"),
        ("last_full_scan", "2026-09-01T00:00:00Z"),
    ] {
        connection
            .execute(
                "insert into settings (key, value) values (?1, ?2)",
                params![key, value],
            )
            .expect("legacy setting saved");
    }
    connection
        .execute(
            "insert into pending_objects
             (kind, source_key, source_hash, payload, shot_source_key, updated_at)
             values ('shot', '42:1700000000', 'legacy-hash', '{\"id\":42}', null, 1)",
            [],
        )
        .expect("legacy upload queued");
}

#[test]
fn legacy_queue_and_identity_survive_repeated_upgrade_starts() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("sync.sqlite");
    seed_single_machine_database(&path);

    let first = AppStore::open(&path).expect("upgraded store opens");
    let machines = first.machines().expect("machine registry read");
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0].id, "11111111-1111-4111-8111-111111111111");
    assert_eq!(machines[0].device_id.as_deref(), Some("legacy-device"));
    assert_eq!(machines[0].name, "GaggiMate");
    assert!(machines[0].legacy_default);
    assert!(machines[0].active);
    assert_eq!(first.pending_count().expect("legacy queue count"), 1);
    assert_eq!(
        first.pending(10).expect("legacy upload read")[0].data,
        json!({ "id": 42 })
    );
    assert_eq!(
        first
            .setting("last_full_scan")
            .expect("scan state read")
            .as_deref(),
        Some("2026-09-01T00:00:00Z")
    );

    let mut renamed = machines[0].clone();
    renamed.name = "Gaggia Büro".into();
    renamed.active = false;
    renamed.pending_detach = true;
    first.save_machine(&renamed).expect("renamed machine saved");
    drop(first);

    let second = AppStore::open(&path).expect("second startup succeeds");
    assert_eq!(second.machines().expect("machines read"), vec![renamed]);
    assert_eq!(second.pending_count().expect("upload still queued"), 1);
    assert_eq!(
        second
            .setting("machine_host")
            .expect("host read")
            .as_deref(),
        Some("office.local")
    );
}

#[test]
fn interrupted_registry_backfill_recovers_on_next_start() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("sync.sqlite");
    seed_single_machine_database(&path);
    // A startup may be interrupted after the new table is created but before
    // its old source is copied into it. Reopening must finish the backfill.
    Connection::open(&path)
        .expect("database opens")
        .execute_batch(
            "create table machines (
               id text primary key, name text not null, device_id text,
               active integer not null default 1,
               pending_detach integer not null default 0,
               legacy_default integer not null default 0
             );",
        )
        .expect("empty registry table created");

    let store = AppStore::open(&path).expect("startup repairs registry");
    assert_eq!(store.machines().expect("machines read").len(), 1);
    assert_eq!(store.pending_count().expect("upload still queued"), 1);
}

#[test]
fn identical_shot_keys_and_retry_state_remain_isolated_by_machine() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let registry_path = directory.path().join("sync.sqlite");
    let extra_path = directory
        .path()
        .join("machine-22222222-2222-4222-8222-222222222222.sqlite");
    let registry = AppStore::open(&registry_path).expect("primary store opens");
    let extra = AppStore::open(&extra_path).expect("second machine store opens");
    let machine = MachineRecord {
        id: "22222222-2222-4222-8222-222222222222".into(),
        name: "Gaggia Büro".into(),
        device_id: Some("office-device".into()),
        active: true,
        pending_detach: false,
        legacy_default: false,
    };
    registry.save_machine(&machine).expect("machine registered");

    let same_key = "42:1700000000";
    let kitchen_shot = pending_shot(same_key, "Kitchen shot");
    let office_shot = pending_shot(same_key, "Office shot");
    registry.queue(&kitchen_shot).expect("first upload queued");
    extra.queue(&office_shot).expect("second upload queued");
    registry
        .record_failure(Some(&kitchen_shot), "shot", same_key, "upload", "offline")
        .expect("first failure saved");
    extra
        .set_setting("last_full_scan", "2026-09-02T00:00:00Z")
        .expect("second scan state saved");
    registry
        .remove_pending("shot", same_key)
        .expect("first upload acknowledged");

    assert_eq!(registry.pending_count().expect("first queue count"), 0);
    assert_eq!(extra.pending_count().expect("second queue count"), 1);
    assert_eq!(registry.failure_count().expect("first failure count"), 1);
    assert_eq!(extra.failure_count().expect("second failure count"), 0);
    assert!(registry
        .setting("last_full_scan")
        .expect("first scan state read")
        .is_none());
    drop(extra);
    drop(registry);

    let registry = AppStore::open(&registry_path).expect("primary store reopens");
    let extra = AppStore::open(&extra_path).expect("second store reopens");
    assert_eq!(registry.machines().expect("registry read"), vec![machine]);
    assert_eq!(registry.failure_count().expect("first failure retained"), 1);
    assert_eq!(
        extra.pending(10).expect("second upload retained")[0].data,
        office_shot.data
    );
    assert_eq!(
        extra
            .setting("last_full_scan")
            .expect("second scan state read")
            .as_deref(),
        Some("2026-09-02T00:00:00Z")
    );
}

#[test]
fn pending_detach_survives_offline_restart_without_erasing_local_uploads() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let registry_path = directory.path().join("sync.sqlite");
    let extra_path = directory
        .path()
        .join("machine-22222222-2222-4222-8222-222222222222.sqlite");
    let registry = AppStore::open(&registry_path).expect("registry opens");
    let extra = AppStore::open(&extra_path).expect("second machine opens");
    let pending = MachineRecord {
        id: "22222222-2222-4222-8222-222222222222".into(),
        name: "Gaggia Büro".into(),
        device_id: Some("office-device".into()),
        active: false,
        pending_detach: true,
        legacy_default: false,
    };
    registry
        .save_machine(&pending)
        .expect("offline detach recorded");
    extra
        .queue(&pending_shot("42:1700000000", "Office shot"))
        .expect("outstanding upload retained");
    drop(extra);
    drop(registry);

    let registry = AppStore::open(&registry_path).expect("registry reopens");
    let extra = AppStore::open(&extra_path).expect("second machine reopens");
    assert_eq!(
        registry.machines().expect("pending detach read"),
        vec![pending]
    );
    assert_eq!(extra.pending_count().expect("outstanding upload read"), 1);
}
