// SPDX-License-Identifier: GPL-3.0-or-later

use std::{path::Path, sync::Mutex};

use keyring::Entry;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use thiserror::Error;

use crate::model::{OAuthTokens, SyncObject};

const KEYRING_SERVICE: &str = "com.mybrewfolio.sync";
const KEYRING_USER: &str = "oauth-tokens";

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

    pub fn clear_account_data(&self) -> Result<(), StoreError> {
        self.connection
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?
            .execute_batch(
                "delete from pending_objects;
                 delete from settings where key not in ('machine_host', 'installation_id');",
            )?;
        Ok(())
    }

    fn keyring() -> Result<Entry, StoreError> {
        Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|_| StoreError::Keychain)
    }

    pub fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), StoreError> {
        let value = serde_json::to_string(tokens).map_err(|_| StoreError::InvalidCredentials)?;
        Self::keyring()?
            .set_password(&value)
            .map_err(|_| StoreError::Keychain)
    }

    pub fn tokens(&self) -> Result<Option<OAuthTokens>, StoreError> {
        match Self::keyring()?.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|_| StoreError::InvalidCredentials),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(StoreError::Keychain),
        }
    }

    pub fn delete_tokens(&self) -> Result<(), StoreError> {
        match Self::keyring()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(StoreError::Keychain),
        }
    }
}
