// SPDX-License-Identifier: GPL-3.0-or-later

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    pub connected: bool,
    pub machine_host: String,
    pub machine_reachable: bool,
    pub syncing: bool,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
    pub profiles: usize,
    pub shots: usize,
    pub notes: usize,
    pub conflicts: usize,
    pub suppressed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEntry {
    pub id: u32,
    pub timestamp: u32,
    pub duration: u32,
    pub volume: Option<f64>,
    pub rating: Option<u8>,
    pub profile_id: String,
    pub profile_name: String,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncObject {
    pub kind: String,
    pub source_key: String,
    pub source_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shot_source_key: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRegistration {
    pub id: String,
    pub source_id: String,
}
