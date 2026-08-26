// SPDX-License-Identifier: GPL-3.0-or-later

//! Credential backends are deliberately separate from SQLite. Desktop builds
//! use the operating-system keychain; headless builds require an explicit
//! encryption key so a mounted Docker volume never contains readable tokens.

use std::{
    fs,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::{model::OAuthTokens, store::StoreError};

pub trait CredentialStore: Send + Sync {
    fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), StoreError>;
    fn tokens(&self) -> Result<Option<OAuthTokens>, StoreError>;
    fn delete_tokens(&self) -> Result<(), StoreError>;
    fn save_pending_device_authorization(&self, value: &str) -> Result<(), StoreError>;
    fn pending_device_authorization(&self) -> Result<Option<String>, StoreError>;
    fn delete_pending_device_authorization(&self) -> Result<(), StoreError>;
}

#[cfg(feature = "desktop")]
pub struct KeyringCredentialStore;

#[cfg(feature = "desktop")]
impl KeyringCredentialStore {
    fn entry() -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new("com.mybrewfolio.sync", "oauth-tokens")
            .map_err(|_| StoreError::Keychain)
    }

    fn pending_entry() -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new("com.mybrewfolio.sync", "oauth-device-pending")
            .map_err(|_| StoreError::Keychain)
    }
}

#[cfg(feature = "desktop")]
impl CredentialStore for KeyringCredentialStore {
    fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), StoreError> {
        let value = serde_json::to_string(tokens).map_err(|_| StoreError::InvalidCredentials)?;
        Self::entry()?
            .set_password(&value)
            .map_err(|_| StoreError::Keychain)
    }

    fn tokens(&self) -> Result<Option<OAuthTokens>, StoreError> {
        match Self::entry()?.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|_| StoreError::InvalidCredentials),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(StoreError::Keychain),
        }
    }

    fn delete_tokens(&self) -> Result<(), StoreError> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(StoreError::Keychain),
        }
    }

    fn save_pending_device_authorization(&self, value: &str) -> Result<(), StoreError> {
        Self::pending_entry()?
            .set_password(value)
            .map_err(|_| StoreError::Keychain)
    }

    fn pending_device_authorization(&self) -> Result<Option<String>, StoreError> {
        match Self::pending_entry()?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(StoreError::Keychain),
        }
    }

    fn delete_pending_device_authorization(&self) -> Result<(), StoreError> {
        match Self::pending_entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(StoreError::Keychain),
        }
    }
}

pub struct EncryptedFileCredentialStore {
    path: PathBuf,
    cipher: XChaCha20Poly1305,
}

#[derive(Default, Serialize, Deserialize)]
struct CredentialEnvelope {
    tokens: Option<OAuthTokens>,
    pending_device_authorization: Option<String>,
}

impl EncryptedFileCredentialStore {
    /// `key_path` contains either exactly 32 raw bytes or their base64 form.
    pub fn from_key_file(path: impl Into<PathBuf>, key_path: &Path) -> Result<Self, StoreError> {
        let bytes = fs::read(key_path).map_err(|_| StoreError::InvalidCredentials)?;
        let trimmed = String::from_utf8_lossy(&bytes).trim().as_bytes().to_vec();
        let key = if bytes.len() == 32 {
            bytes
        } else {
            STANDARD
                .decode(trimmed)
                .map_err(|_| StoreError::InvalidCredentials)?
        };
        let key: [u8; 32] = key.try_into().map_err(|_| StoreError::InvalidCredentials)?;
        Ok(Self {
            path: path.into(),
            cipher: XChaCha20Poly1305::new((&key).into()),
        })
    }

    fn write_private(&self, bytes: &[u8]) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|_| StoreError::InvalidCredentials)?;
        }
        let temporary = self.path.with_extension("enc.tmp");
        fs::write(&temporary, bytes).map_err(|_| StoreError::InvalidCredentials)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(|_| StoreError::InvalidCredentials)?;
        }
        fs::rename(temporary, &self.path).map_err(|_| StoreError::InvalidCredentials)
    }

    fn read_envelope(&self) -> Result<CredentialEnvelope, StoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CredentialEnvelope::default())
            }
            Err(_) => return Err(StoreError::InvalidCredentials),
        };
        if bytes.len() < 24 {
            return Err(StoreError::InvalidCredentials);
        }
        let (nonce, ciphertext) = bytes.split_at(24);
        let plaintext = self
            .cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| StoreError::InvalidCredentials)?;
        serde_json::from_slice(&plaintext).map_err(|_| StoreError::InvalidCredentials)
    }

    fn write_envelope(&self, envelope: &CredentialEnvelope) -> Result<(), StoreError> {
        let plaintext = serde_json::to_vec(envelope).map_err(|_| StoreError::InvalidCredentials)?;
        let mut nonce = [0_u8; 24];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext.as_ref())
            .map_err(|_| StoreError::InvalidCredentials)?;
        let mut encoded = nonce.to_vec();
        encoded.extend(ciphertext);
        self.write_private(&encoded)
    }
}

impl CredentialStore for EncryptedFileCredentialStore {
    fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), StoreError> {
        let mut envelope = self.read_envelope()?;
        envelope.tokens = Some(tokens.clone());
        self.write_envelope(&envelope)
    }

    fn tokens(&self) -> Result<Option<OAuthTokens>, StoreError> {
        Ok(self.read_envelope()?.tokens)
    }

    fn delete_tokens(&self) -> Result<(), StoreError> {
        let mut envelope = self.read_envelope()?;
        envelope.tokens = None;
        self.write_envelope(&envelope)
    }

    fn save_pending_device_authorization(&self, value: &str) -> Result<(), StoreError> {
        let mut envelope = self.read_envelope()?;
        envelope.pending_device_authorization = Some(value.to_string());
        self.write_envelope(&envelope)
    }

    fn pending_device_authorization(&self) -> Result<Option<String>, StoreError> {
        Ok(self.read_envelope()?.pending_device_authorization)
    }

    fn delete_pending_device_authorization(&self) -> Result<(), StoreError> {
        let mut envelope = self.read_envelope()?;
        envelope.pending_device_authorization = None;
        self.write_envelope(&envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::{CredentialStore, EncryptedFileCredentialStore};
    use crate::model::OAuthTokens;

    #[test]
    fn encrypted_file_never_contains_token_text() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let key = directory.path().join("key");
        std::fs::write(&key, [7_u8; 32]).expect("key written");
        let path = directory.path().join("credentials.enc");
        let store = EncryptedFileCredentialStore::from_key_file(&path, &key).expect("store opens");
        store
            .save_tokens(&OAuthTokens {
                access_token: "secret-access-token".into(),
                refresh_token: None,
                expires_at: 1,
            })
            .expect("saved");
        assert!(
            !String::from_utf8_lossy(&std::fs::read(&path).expect("read"))
                .contains("secret-access-token")
        );
        assert_eq!(
            store.tokens().expect("read").expect("token").access_token,
            "secret-access-token"
        );
    }

    #[test]
    fn encrypted_store_keeps_tokens_and_pairing_state_independent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let key = directory.path().join("key");
        std::fs::write(&key, [9_u8; 32]).expect("key written");
        let path = directory.path().join("credentials.enc");
        let store = EncryptedFileCredentialStore::from_key_file(&path, &key).expect("store opens");
        let tokens = OAuthTokens {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: 42,
        };

        assert!(store.tokens().expect("empty tokens").is_none());
        store.save_tokens(&tokens).expect("tokens saved");
        store
            .save_pending_device_authorization("pairing-state")
            .expect("pairing state saved");
        assert_eq!(
            store
                .tokens()
                .expect("tokens read")
                .as_ref()
                .map(|value| &value.access_token),
            Some(&"access".into())
        );
        assert_eq!(
            store
                .pending_device_authorization()
                .expect("pairing state read"),
            Some("pairing-state".into())
        );

        store.delete_tokens().expect("tokens deleted");
        assert!(store.tokens().expect("empty tokens").is_none());
        assert_eq!(
            store
                .pending_device_authorization()
                .expect("pairing state retained"),
            Some("pairing-state".into())
        );
        store
            .delete_pending_device_authorization()
            .expect("pairing state deleted");
        assert_eq!(
            store
                .pending_device_authorization()
                .expect("empty pairing state"),
            None
        );
    }
}
