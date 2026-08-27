// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use reqwest::redirect::Policy;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{client_async, tungstenite::Message};
use url::Url;
use uuid::Uuid;

use crate::{
    binary::{parse_index, parse_shot},
    model::IndexEntry,
};

#[derive(Debug, Error)]
pub enum LocalError {
    #[error("Use gaggimate.local or a private local IP address")]
    InvalidHost,
    #[error("The GaggiMate could not be reached")]
    Unreachable,
    #[error("The GaggiMate returned invalid data")]
    InvalidData,
}

fn private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_unicast_link_local() || ip.is_loopback(),
    }
}

fn saved_profile_id(response: &Value, requested_id: &str) -> String {
    response
        .get("profile")
        .and_then(|saved| saved.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .unwrap_or(requested_id)
        .to_string()
}

struct LocalTarget {
    authority: String,
    host: String,
    port: u16,
}

fn local_target(value: &str) -> Result<LocalTarget, LocalError> {
    let input = value.trim().trim_end_matches('.').to_lowercase();
    if input.is_empty()
        || input.contains("//")
        || input.contains('/')
        || input.contains('?')
        || input.contains('#')
        || input.contains('@')
    {
        return Err(LocalError::InvalidHost);
    }
    if let Ok(address) = input.parse::<IpAddr>() {
        if !private_address(address) {
            return Err(LocalError::InvalidHost);
        }
        let host = address.to_string();
        let authority = match address {
            IpAddr::V4(_) => host.clone(),
            IpAddr::V6(_) => format!("[{host}]"),
        };
        return Ok(LocalTarget {
            authority,
            host,
            port: 80,
        });
    }
    let url = Url::parse(&format!("http://{input}/")).map_err(|_| LocalError::InvalidHost)?;
    let host = url
        .host_str()
        .ok_or(LocalError::InvalidHost)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
    let allowed =
        host == "gaggimate.local" || host.parse::<IpAddr>().map(private_address).unwrap_or(false);
    if !allowed {
        return Err(LocalError::InvalidHost);
    }
    let port = url.port().unwrap_or(80);
    if port == 0 {
        return Err(LocalError::InvalidHost);
    }
    let authority = match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]:{port}"),
        _ if port == 80 => host.clone(),
        _ => format!("{host}:{port}"),
    };
    Ok(LocalTarget {
        authority,
        host,
        port,
    })
}

pub fn normalize_host(value: &str) -> Result<String, LocalError> {
    Ok(local_target(value)?.authority)
}

#[derive(Clone)]
pub struct GaggiMateClient {
    host: String,
    authority: String,
    port: u16,
    socket_address: SocketAddr,
    http: reqwest::Client,
}

impl GaggiMateClient {
    pub fn new(host: &str) -> Result<Self, LocalError> {
        let target = local_target(host)?;
        let addresses = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(|_| LocalError::Unreachable)?
            .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses
                .iter()
                .any(|address| !private_address(address.ip()))
        {
            return Err(LocalError::InvalidHost);
        }
        let socket_address = addresses[0];
        let mut builder = reqwest::Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20));
        if target.host.parse::<IpAddr>().is_err() {
            // Pin the already validated private result. This prevents a later
            // DNS or hosts-file change from redirecting the HTTP request to a
            // public address during the same Sync cycle.
            builder = builder.resolve(&target.host, socket_address);
        }
        let http = builder.build().map_err(|_| LocalError::Unreachable)?;
        Ok(Self {
            host: target.host,
            authority: target.authority,
            port: target.port,
            socket_address,
            http,
        })
    }

    fn http_url(&self, path: &str) -> String {
        format!("http://{}{}", self.authority, path)
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.authority)
    }

    fn response_is_local(&self, response: &reqwest::Response) -> bool {
        response
            .url()
            .host_str()
            .map(|host| host.trim_matches(['[', ']']))
            == Some(self.host.as_str())
            && response.url().port_or_known_default() == Some(self.port)
    }

    async fn bytes(&self, path: &str) -> Result<Vec<u8>, LocalError> {
        let response = self
            .http
            .get(self.http_url(path))
            .send()
            .await
            .map_err(|_| LocalError::Unreachable)?;
        if !response.status().is_success() || !self.response_is_local(&response) {
            return Err(LocalError::Unreachable);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| LocalError::InvalidData)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err(LocalError::InvalidData);
        }
        Ok(bytes.to_vec())
    }

    pub async fn shot_index(&self) -> Result<Vec<IndexEntry>, LocalError> {
        parse_index(&self.bytes("/api/history/index.bin").await?)
            .map_err(|_| LocalError::InvalidData)
    }

    pub async fn shot(&self, id: u32) -> Result<Value, LocalError> {
        let path = format!("/api/history/{id:06}.slog");
        parse_shot(&self.bytes(&path).await?, id).map_err(|_| LocalError::InvalidData)
    }

    pub async fn notes(&self, id: u32) -> Result<Option<Value>, LocalError> {
        let response = self
            .websocket_request_allowing_error(
                "req:history:notes:get",
                json!({ "id": id.to_string() }),
            )
            .await?;
        // Real GaggiMate versions may report a missing notes record as a
        // protocol-level error instead of returning an empty object.
        if response.get("error").is_some() {
            return Ok(None);
        }
        let Some(notes) = response.get("notes").cloned() else {
            return Ok(None);
        };
        if notes.is_null() {
            return Ok(None);
        }
        let object = notes.as_object().ok_or(LocalError::InvalidData)?;
        if object.is_empty() {
            return Ok(None);
        }
        if serde_json::to_vec(&notes)
            .map_err(|_| LocalError::InvalidData)?
            .len()
            > 64 * 1024
        {
            return Err(LocalError::InvalidData);
        }
        Ok(Some(notes))
    }

    pub async fn save_notes(&self, id: u32, notes: &Value) -> Result<Value, LocalError> {
        let object = notes.as_object().ok_or(LocalError::InvalidData)?;
        if serde_json::to_vec(notes)
            .map_err(|_| LocalError::InvalidData)?
            .len()
            > 64 * 1024
        {
            return Err(LocalError::InvalidData);
        }
        self.websocket_request(
            "req:history:notes:save",
            json!({ "id": id.to_string(), "notes": object }),
        )
        .await?;
        Ok(self.notes(id).await?.unwrap_or_else(|| json!({})))
    }

    async fn websocket_request(
        &self,
        request_type: &str,
        body: Value,
    ) -> Result<Value, LocalError> {
        self.websocket_request_inner(request_type, body, false)
            .await
    }

    async fn websocket_request_allowing_error(
        &self,
        request_type: &str,
        body: Value,
    ) -> Result<Value, LocalError> {
        self.websocket_request_inner(request_type, body, true).await
    }

    async fn websocket_request_inner(
        &self,
        request_type: &str,
        body: Value,
        allow_error: bool,
    ) -> Result<Value, LocalError> {
        let url = Url::parse(&self.ws_url()).map_err(|_| LocalError::InvalidHost)?;
        let stream = tokio::time::timeout(
            Duration::from_secs(20),
            TcpStream::connect(self.socket_address),
        )
        .await
        .map_err(|_| LocalError::Unreachable)?
        .map_err(|_| LocalError::Unreachable)?;
        // Use the original local hostname in the WebSocket handshake while
        // connecting to the validated, pinned private socket address.
        let (mut socket, _) = client_async(url.as_str(), stream)
            .await
            .map_err(|_| LocalError::Unreachable)?;
        let rid = Uuid::new_v4().to_string();
        let mut request = body.as_object().cloned().unwrap_or_default();
        request.insert("tp".into(), json!(request_type));
        request.insert("rid".into(), json!(rid));
        socket
            .send(Message::Text(Value::Object(request).to_string().into()))
            .await
            .map_err(|_| LocalError::Unreachable)?;
        let expected = format!("res:{}", request_type.trim_start_matches("req:"));
        let response = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(message) = socket.next().await {
                let message = message.map_err(|_| LocalError::Unreachable)?;
                let text = message.into_text().map_err(|_| LocalError::InvalidData)?;
                if text.len() > 1024 * 1024 {
                    return Err(LocalError::InvalidData);
                }
                let value: Value =
                    serde_json::from_str(&text).map_err(|_| LocalError::InvalidData)?;
                let response_type = value.get("tp").and_then(Value::as_str);
                if value.get("rid").and_then(Value::as_str) == Some(&rid)
                    && (response_type == Some(&expected) || response_type == Some("res:error"))
                {
                    if value.get("error").is_some() && !allow_error {
                        return Err(LocalError::InvalidData);
                    }
                    return Ok(value);
                }
            }
            Err(LocalError::Unreachable)
        })
        .await
        .map_err(|_| LocalError::Unreachable)??;
        let _ = socket.close(None).await;
        Ok(response)
    }

    pub async fn profiles(&self) -> Result<Vec<(String, Result<Value, LocalError>)>, LocalError> {
        let listing = self
            .websocket_request("req:profiles:list", json!({ "minimal": true }))
            .await?;
        let profiles = listing
            .get("profiles")
            .and_then(Value::as_array)
            .ok_or(LocalError::InvalidData)?;
        let mut result = Vec::with_capacity(profiles.len());
        for profile in profiles {
            let id = profile
                .get("id")
                .and_then(Value::as_str)
                .ok_or(LocalError::InvalidData)?
                .to_string();
            let loaded = self
                .websocket_request("req:profiles:load", json!({ "id": id }))
                .await
                .and_then(|value| value.get("profile").cloned().ok_or(LocalError::InvalidData));
            result.push((id, loaded));
        }
        Ok(result)
    }

    pub async fn profile_inventory(&self) -> Result<Vec<Value>, LocalError> {
        let listing = self
            .websocket_request("req:profiles:list", json!({ "minimal": true }))
            .await?;
        let profiles = listing
            .get("profiles")
            .and_then(Value::as_array)
            .ok_or(LocalError::InvalidData)?;
        profiles
            .iter()
            .map(|profile| {
                let id = profile
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 128)
                    .ok_or(LocalError::InvalidData)?;
                let label = profile
                    .get("label")
                    .and_then(Value::as_str)
                    .filter(|label| !label.is_empty() && label.len() <= 240)
                    .ok_or(LocalError::InvalidData)?;
                Ok(json!({
                    "id": id,
                    "label": label,
                    "favorite": profile.get("favorite").and_then(Value::as_bool).unwrap_or(false),
                    "selected": profile.get("selected").and_then(Value::as_bool).unwrap_or(false),
                }))
            })
            .collect()
    }

    pub async fn load_profile(&self, id: &str) -> Result<Value, LocalError> {
        if id.is_empty() || id.len() > 128 {
            return Err(LocalError::InvalidData);
        }
        let response = self
            .websocket_request("req:profiles:load", json!({ "id": id }))
            .await?;
        let profile = response
            .get("profile")
            .cloned()
            .ok_or(LocalError::InvalidData)?;
        if !profile.is_object()
            || serde_json::to_vec(&profile)
                .map_err(|_| LocalError::InvalidData)?
                .len()
                > 1024 * 1024
        {
            return Err(LocalError::InvalidData);
        }
        Ok(profile)
    }

    pub async fn save_profile(&self, profile: &Value) -> Result<String, LocalError> {
        let mut safe = profile
            .as_object()
            .cloned()
            .ok_or(LocalError::InvalidData)?;
        if serde_json::to_vec(&safe)
            .map_err(|_| LocalError::InvalidData)?
            .len()
            > 1024 * 1024
        {
            return Err(LocalError::InvalidData);
        }
        safe.remove("favorite");
        safe.remove("selected");
        let requested_id = safe
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or(LocalError::InvalidData)?
            .to_string();
        let response = self
            .websocket_request("req:profiles:save", json!({ "profile": safe }))
            .await?;
        // IDs are firmware-managed: when the machine assigns a fresh ID, all
        // following load/favorite/select operations must use that ID rather
        // than the Store draft's requested one.
        Ok(saved_profile_id(&response, &requested_id))
    }

    pub async fn favorite_profile(&self, id: &str) -> Result<(), LocalError> {
        self.websocket_request("req:profiles:favorite", json!({ "id": id }))
            .await?;
        Ok(())
    }

    pub async fn select_profile(&self, id: &str) -> Result<(), LocalError> {
        self.websocket_request("req:profiles:select", json!({ "id": id }))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_machine_targets_are_allowed() {
        assert_eq!(
            normalize_host("gaggimate.local").unwrap(),
            "gaggimate.local"
        );
        assert!(normalize_host("192.168.1.23").is_ok());
        assert_eq!(normalize_host("127.0.0.1:8088").unwrap(), "127.0.0.1:8088");
        assert_eq!(normalize_host("[fd00::1]:8088").unwrap(), "[fd00::1]:8088");
        assert!(normalize_host("8.8.8.8").is_err());
        assert!(normalize_host("8.8.8.8:8088").is_err());
        assert!(normalize_host("example.com").is_err());
        assert!(normalize_host("https://gaggimate.local").is_err());
    }

    #[test]
    fn host_normalization_preserves_only_a_safe_local_authority() {
        assert_eq!(
            normalize_host("  GaggiMate.Local. ").expect("local hostname"),
            "gaggimate.local"
        );
        assert_eq!(
            normalize_host("192.168.178.140:8080").expect("private address"),
            "192.168.178.140:8080"
        );
        assert_eq!(
            normalize_host("[fd00::42]").expect("private IPv6"),
            "[fd00::42]:80"
        );
        for invalid in [
            "",
            "gaggimate.local/path",
            "gaggimate.local?next=example",
            "user@gaggimate.local",
            "gaggimate.local#fragment",
            "gaggimate.local//other",
            "[2001:4860:4860::8888]",
        ] {
            assert!(
                normalize_host(invalid).is_err(),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn local_client_only_resolves_an_already_validated_private_target() {
        let client = GaggiMateClient::new("127.0.0.1:1").expect("loopback target");
        assert_eq!(
            client.http_url("/api/history/index.bin"),
            "http://127.0.0.1:1/api/history/index.bin"
        );
        assert_eq!(client.ws_url(), "ws://127.0.0.1:1/ws");
        assert!(matches!(
            GaggiMateClient::new("public.example.test"),
            Err(LocalError::InvalidHost)
        ));
    }

    #[test]
    fn save_response_uses_the_firmware_assigned_profile_id() {
        assert_eq!(
            saved_profile_id(
                &json!({ "profile": { "id": "machine-profile" } }),
                "store-profile"
            ),
            "machine-profile"
        );
        assert_eq!(
            saved_profile_id(&json!({}), "store-profile"),
            "store-profile"
        );
    }
}
