// SPDX-License-Identifier: GPL-3.0-or-later

use std::{path::Path, sync::Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use thiserror::Error;

use crate::model::SyncObject;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("The local Sync database could not be opened")]
    Database(#[from] rusqlite::Error),
    #[error("The operating system keychain is unavailable")]
    Keychain,
    #[error("Stored account credentials are invalid")]
    InvalidCredentials,
}

pub struct AppStore {
    connection: Mutex<Connection>,
}

impl AppStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| StoreError::InvalidCredentials)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "pragma journal_mode = wal;
             create table if not exists settings (
               key text primary key,
               value text not null
             );
             create table if not exists pending_objects (
               kind text not null,
               source_key text not null,
               source_hash text not null,
               payload text not null,
               shot_source_key text,
               updated_at integer not null,
               primary key (kind, source_key)
             );
             create table if not exists sync_failures (
               kind text not null,
               source_key text not null,
               stage text not null,
               reason text not null,
               payload text,
               source_hash text,
               shot_source_key text,
               attempts integer not null default 1,
               updated_at integer not null,
               primary key (kind, source_key, stage)
             );",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        connection
            .query_row("select value from settings where key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        connection.execute(
            "insert into settings (key, value) values (?1, ?2)
             on conflict (key) do update set value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn remove_setting(&self, key: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?
            .execute("delete from settings where key = ?1", [key])?;
        Ok(())
    }

    pub fn queue(&self, object: &SyncObject) -> Result<(), StoreError> {
        let payload =
            serde_json::to_string(&object.data).map_err(|_| StoreError::InvalidCredentials)?;
        self.connection.lock().map_err(|_| StoreError::InvalidCredentials)?.execute(
            "insert into pending_objects (kind, source_key, source_hash, payload, shot_source_key, updated_at)
             values (?1, ?2, ?3, ?4, ?5, unixepoch())
             on conflict (kind, source_key) do update
               set source_hash = excluded.source_hash, payload = excluded.payload,
                   shot_source_key = excluded.shot_source_key, updated_at = unixepoch()",
            params![object.kind, object.source_key, object.source_hash, payload, object.shot_source_key],
        )?;
        Ok(())
    }

    pub fn pending(&self, limit: usize) -> Result<Vec<SyncObject>, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        let mut statement = connection.prepare(
            "select kind, source_key, source_hash, payload, shot_source_key
             from pending_objects
             order by case kind when 'profile' then 0 when 'shot' then 1 else 2 end, updated_at
             limit ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            let payload: String = row.get(3)?;
            Ok(SyncObject {
                kind: row.get(0)?,
                source_key: row.get(1)?,
                source_hash: row.get(2)?,
                data: serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
                shot_source_key: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn pending_count(&self) -> Result<usize, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        let count: i64 =
            connection.query_row("select count(*) from pending_objects", [], |row| row.get(0))?;
        Ok(count.max(0) as usize)
    }

    pub fn remove_pending(&self, kind: &str, source_key: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?
            .execute(
                "delete from pending_objects where kind = ?1 and source_key = ?2",
                params![kind, source_key],
            )?;
        Ok(())
    }

    pub fn record_failure(
        &self,
        object: Option<&SyncObject>,
        kind: &str,
        source_key: &str,
        stage: &str,
        reason: &str,
    ) -> Result<(), StoreError> {
        // Notes can contain private free text. Diagnostics retain only their
        // source identity and retry metadata; a retry reads them from the
        // machine again instead of persisting the content here.
        let payload = object
            .filter(|value| value.kind != "notes")
            .map(|value| serde_json::to_string(&value.data))
            .transpose()
            .map_err(|_| StoreError::InvalidCredentials)?;
        self.connection.lock().map_err(|_| StoreError::InvalidCredentials)?.execute(
            "insert into sync_failures (
               kind, source_key, stage, reason, payload, source_hash,
               shot_source_key, attempts, updated_at
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, unixepoch())
             on conflict (kind, source_key, stage) do update
               set reason = excluded.reason,
                   payload = coalesce(excluded.payload, sync_failures.payload),
                   source_hash = coalesce(excluded.source_hash, sync_failures.source_hash),
                   shot_source_key = coalesce(excluded.shot_source_key, sync_failures.shot_source_key),
                   attempts = sync_failures.attempts + 1,
                   updated_at = unixepoch()",
            params![
                kind,
                source_key,
                stage,
                reason,
                payload,
                object.map(|value| value.source_hash.as_str()),
                object.and_then(|value| value.shot_source_key.as_deref()),
            ],
        )?;
        Ok(())
    }

    pub fn clear_failure(&self, kind: &str, source_key: &str) -> Result<(), StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?
            .execute(
                "delete from sync_failures where kind = ?1 and source_key = ?2",
                params![kind, source_key],
            )?;
        Ok(())
    }

    pub fn failures(&self) -> Result<Vec<crate::model::SyncIssue>, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        let mut statement = connection.prepare(
            "select kind, source_key, stage, reason, attempts, updated_at
             from sync_failures order by updated_at desc limit 100",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(crate::model::SyncIssue {
                kind: row.get(0)?,
                source_key: row.get(1)?,
                stage: row.get(2)?,
                reason: row.get(3)?,
                attempts: row.get::<_, i64>(4)?.max(0) as u32,
                updated_at: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn failure_count(&self) -> Result<usize, StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        let count: i64 =
            connection.query_row("select count(*) from sync_failures", [], |row| row.get(0))?;
        Ok(count.max(0) as usize)
    }

    pub fn retry_failures(&self) -> Result<(), StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        connection.execute_batch(
            "insert into pending_objects (
               kind, source_key, source_hash, payload, shot_source_key, updated_at
             )
             select kind, source_key, source_hash, payload, shot_source_key, unixepoch()
             from sync_failures
             where payload is not null and source_hash is not null
             on conflict (kind, source_key) do update
               set source_hash = excluded.source_hash,
                   payload = excluded.payload,
                   shot_source_key = excluded.shot_source_key,
                   updated_at = unixepoch();
             delete from settings
             where key like 'shot_fingerprint_v2:%'
                or key in ('last_profile_scan', 'last_recent_notes_scan',
                           'last_full_notes_scan');
             delete from sync_failures;",
        )?;
        Ok(())
    }

    pub fn reset_scan_state(&self) -> Result<(), StoreError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?;
        connection.execute("delete from pending_objects", [])?;
        connection.execute("delete from sync_failures", [])?;
        connection.execute(
            "delete from settings
             where key like 'shot_fingerprint_%'
                or key in ('last_profile_scan', 'last_recent_notes_scan',
                           'last_full_notes_scan', 'notes_reader_version')",
            [],
        )?;
        Ok(())
    }

    pub fn clear_account_data(&self) -> Result<(), StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?
            .execute_batch(
                "delete from pending_objects;
                 delete from sync_failures;
                 delete from settings where key not in ('machine_host', 'installation_id', 'hide_app_icon');",
            )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AppStore;
    use crate::model::SyncObject;
    use serde_json::json;

    fn object(kind: &str, source_key: &str) -> SyncObject {
        SyncObject {
            kind: kind.into(),
            source_key: source_key.into(),
            source_hash: format!("hash-{source_key}"),
            shot_source_key: (kind == "notes").then(|| "shot:1".into()),
            data: json!({ "name": source_key, "notes": "private" }),
        }
    }

    #[test]
    fn clearing_account_data_keeps_machine_and_installation_settings() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = AppStore::open(&directory.path().join("sync.sqlite")).expect("store opens");
        store
            .set_setting("machine_host", "gaggimate.local")
            .expect("machine host saved");
        store
            .set_setting("installation_id", "stable-installation")
            .expect("installation ID saved");
        store
            .set_setting("hide_app_icon", "1")
            .expect("app icon setting saved");
        store
            .set_setting("device_id", "account-device")
            .expect("device ID saved");
        store
            .set_setting("source_id", "account-source")
            .expect("source ID saved");

        store.clear_account_data().expect("account data cleared");

        assert_eq!(
            store.setting("machine_host").expect("machine host read"),
            Some("gaggimate.local".into())
        );
        assert_eq!(
            store
                .setting("installation_id")
                .expect("installation ID read"),
            Some("stable-installation".into())
        );
        assert_eq!(
            store
                .setting("hide_app_icon")
                .expect("app icon setting read"),
            Some("1".into())
        );
        assert_eq!(store.setting("device_id").expect("device ID read"), None);
        assert_eq!(store.setting("source_id").expect("source ID read"), None);
    }

    #[test]
    fn queue_and_failure_retry_preserve_sync_data_but_never_note_contents() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = AppStore::open(&directory.path().join("sync.sqlite")).expect("store opens");
        let profile = object("profile", "profile-1");
        let notes = object("notes", "shot:1");
        store.queue(&notes).expect("notes queued");
        store.queue(&profile).expect("profile queued");
        assert_eq!(store.pending_count().expect("pending count"), 2);
        assert_eq!(store.pending(10).expect("pending items")[0].kind, "profile");

        store
            .record_failure(Some(&profile), "profile", "profile-1", "upload", "rejected")
            .expect("profile failure recorded");
        store
            .record_failure(Some(&notes), "notes", "shot:1", "read", "unavailable")
            .expect("notes failure recorded");
        assert_eq!(store.failure_count().expect("failure count"), 2);
        store
            .remove_pending("profile", "profile-1")
            .expect("profile removed");
        store
            .remove_pending("notes", "shot:1")
            .expect("notes removed");

        store.retry_failures().expect("failures retried");
        let pending = store.pending(10).expect("retried pending items");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, "profile");
        assert_eq!(store.failure_count().expect("failures cleared"), 0);
    }

    #[test]
    fn reset_scan_state_removes_sync_cache_without_erasing_machine_settings() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = AppStore::open(&directory.path().join("sync.sqlite")).expect("store opens");
        store
            .set_setting("machine_host", "gaggimate.local")
            .expect("host saved");
        store
            .set_setting("shot_fingerprint_v2:1:2", "fingerprint")
            .expect("fingerprint saved");
        store
            .set_setting("last_full_notes_scan", "2026-08-01T00:00:00Z")
            .expect("scan saved");
        store.queue(&object("shot", "1:2")).expect("queued");
        store
            .record_failure(None, "shot", "1:2", "read", "failed")
            .expect("failed");

        store.reset_scan_state().expect("scan state reset");

        assert_eq!(
            store.setting("machine_host").expect("host read"),
            Some("gaggimate.local".into())
        );
        assert_eq!(
            store
                .setting("shot_fingerprint_v2:1:2")
                .expect("fingerprint cleared"),
            None
        );
        assert_eq!(
            store.setting("last_full_notes_scan").expect("scan cleared"),
            None
        );
        assert_eq!(store.pending_count().expect("pending cleared"), 0);
        assert_eq!(store.failure_count().expect("failures cleared"), 0);
    }

    #[test]
    fn settings_and_pending_objects_are_updated_without_duplicate_rows() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = AppStore::open(&directory.path().join("sync.sqlite")).expect("store opens");
        store
            .set_setting("machine_host", "first.local")
            .expect("setting saved");
        store
            .set_setting("machine_host", "second.local")
            .expect("setting updated");
        assert_eq!(
            store.setting("machine_host").expect("setting read"),
            Some("second.local".into())
        );
        store
            .remove_setting("machine_host")
            .expect("setting removed");
        assert!(store
            .setting("machine_host")
            .expect("setting absent")
            .is_none());

        let mut shot = object("shot", "1:2");
        store.queue(&shot).expect("shot queued");
        shot.source_hash = "new-hash".into();
        shot.data = json!({ "name": "new value" });
        store.queue(&shot).expect("shot updated");
        let pending = store.pending(10).expect("pending read");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].source_hash, "new-hash");
        assert_eq!(pending[0].data["name"], "new value");
    }

    #[test]
    fn failures_keep_retry_metadata_and_can_be_cleared_individually() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = AppStore::open(&directory.path().join("sync.sqlite")).expect("store opens");
        let shot = object("shot", "1:2");
        store
            .record_failure(Some(&shot), "shot", "1:2", "upload", "first failure")
            .expect("first failure recorded");
        store
            .record_failure(Some(&shot), "shot", "1:2", "upload", "second failure")
            .expect("second failure recorded");

        let failures = store.failures().expect("failures read");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].attempts, 2);
        assert_eq!(failures[0].reason, "second failure");

        store.clear_failure("shot", "1:2").expect("failure cleared");
        assert!(store.failures().expect("empty failures").is_empty());
    }
}
