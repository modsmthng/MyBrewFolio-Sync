// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::{Arc, Mutex as StdMutex};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    time::{timeout, Duration},
};

use super::MachineManager;
use crate::{
    cloud::CloudClient,
    credentials::CredentialStore,
    model::OAuthTokens,
    store::{AppStore, MachineRecord, StoreError},
};

struct TestCredentials(StdMutex<Option<OAuthTokens>>);

impl CredentialStore for TestCredentials {
    fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), StoreError> {
        *self.0.lock().expect("credentials lock") = Some(tokens.clone());
        Ok(())
    }

    fn tokens(&self) -> Result<Option<OAuthTokens>, StoreError> {
        Ok(self.0.lock().expect("credentials lock").clone())
    }

    fn delete_tokens(&self) -> Result<(), StoreError> {
        *self.0.lock().expect("credentials lock") = None;
        Ok(())
    }

    fn save_pending_device_authorization(&self, _value: &str) -> Result<(), StoreError> {
        Ok(())
    }

    fn pending_device_authorization(&self) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    fn delete_pending_device_authorization(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[tokio::test]
async fn new_installation_does_not_bind_an_existing_account_machine_during_login() {
    let installation_id = "22222222-2222-4222-8222-222222222222";
    let remote_machine = "11111111-1111-4111-8111-111111111111";
    let directory = tempfile::tempdir().expect("temporary directory");
    let registry = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).expect("store"));
    registry
        .set_setting("installation_id", installation_id)
        .expect("installation ID");
    registry
        .set_setting("machine_host", "192.168.1.45")
        .expect("local address");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let url = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let observed = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("request accepted");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).await.expect("request read");
                assert!(count > 0, "request ended before headers");
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let first_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .expect("request line")
                .to_owned();
            observed
                .lock()
                .expect("request log")
                .push(first_line.clone());
            let body = if first_line.starts_with("GET ") {
                format!("{{\"machines\":[{{\"id\":\"{remote_machine}\",\"name\":\"Existing\",\"legacyDefault\":true}}]}}")
            } else {
                format!("{{\"installationId\":\"{installation_id}\",\"capabilities\":{{\"multiMachine\":1}}}}")
            };
            let response = format!("HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response written");
        }
    });
    let credentials: Arc<dyn CredentialStore> =
        Arc::new(TestCredentials(StdMutex::new(Some(OAuthTokens {
            access_token: "valid".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        }))));
    let mut cloud = CloudClient::new(credentials.clone()).expect("cloud client");
    cloud.config.api_url = url;
    let manager = MachineManager::open_with_cloud(
        directory.path(),
        registry.clone(),
        credentials,
        Arc::new(cloud),
    )
    .expect("machine manager");
    manager
        .after_oauth_authorized(Some("New Gaggia".into()))
        .await
        .expect("account authorized");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("server finished")
        .expect("server task");
    assert!(registry.machines().expect("machine registry").is_empty());
    assert!(registry.setting("source_id").expect("source ID").is_none());
    let status = manager.status_json().await;
    assert_eq!(status["connected"], true);
    assert_eq!(status["machines"].as_array().map(Vec::len), Some(0));
    let lines = requests.lock().expect("request log");
    assert!(lines[0].starts_with("POST "));
    assert!(lines[1].starts_with("GET "));
}

#[tokio::test]
async fn reconnect_waits_for_pending_detach_and_preserves_new_attachment() {
    let machine_id = "11111111-1111-4111-8111-111111111111";
    let installation_id = "22222222-2222-4222-8222-222222222222";
    let directory = tempfile::tempdir().expect("temporary directory");
    let registry = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).expect("store"));
    registry
        .set_setting("installation_id", installation_id)
        .expect("installation ID");
    registry
        .save_machine(&MachineRecord {
            id: machine_id.into(),
            name: "Office".into(),
            device_id: Some("old-device".into()),
            active: false,
            pending_detach: true,
            legacy_default: true,
        })
        .expect("pending removal");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let url = format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    );
    let (detach_started, detach_started_rx) = oneshot::channel();
    let (release_detach, release_detach_rx) = oneshot::channel();
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let observed_server = observed.clone();
    let machine_id_server = machine_id.to_owned();
    let server = tokio::spawn(async move {
        let mut started = Some(detach_started);
        let mut release = Some(release_detach_rx);
        let mut handlers = Vec::new();
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.expect("request accepted");
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 4096];
                let count = stream.read(&mut chunk).await.expect("request read");
                assert!(count > 0, "request ended before headers");
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let first_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .expect("request line")
                .to_owned();
            observed_server
                .lock()
                .expect("request log")
                .push(first_line.clone());
            let machine_id = machine_id_server.clone();
            let wait_for_release = if first_line.starts_with("DELETE ") {
                started
                    .take()
                    .expect("one detach")
                    .send(())
                    .expect("detach signal");
                release.take()
            } else {
                None
            };
            handlers.push(tokio::spawn(async move {
                if let Some(wait_for_release) = wait_for_release {
                    wait_for_release.await.expect("detach released");
                }
                let (status, body) = if first_line.starts_with("DELETE ") {
                    ("204 No Content", String::new())
                } else if first_line.starts_with("GET ") {
                    (
                        "200 OK",
                        format!("{{\"machines\":[{{\"id\":\"{machine_id}\",\"name\":\"Office\",\"legacyDefault\":true}}]}}"),
                    )
                } else {
                    (
                        "200 OK",
                        format!("{{\"device\":{{\"id\":\"new-device\",\"sourceId\":\"{machine_id}\"}}}}"),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("response written");
            }));
        }
        for handler in handlers {
            handler.await.expect("response task");
        }
    });

    let credentials: Arc<dyn CredentialStore> =
        Arc::new(TestCredentials(StdMutex::new(Some(OAuthTokens {
            access_token: "valid".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        }))));
    let mut cloud = CloudClient::new(credentials.clone()).expect("cloud client");
    cloud.config.api_url = url;
    let manager = Arc::new(
        MachineManager::open_with_cloud(
            directory.path(),
            registry.clone(),
            credentials,
            Arc::new(cloud),
        )
        .expect("machine manager"),
    );

    let flush_manager = manager.clone();
    let flush = tokio::spawn(async move { flush_manager.flush_pending_detaches().await });
    timeout(Duration::from_secs(5), detach_started_rx)
        .await
        .expect("detach requested")
        .expect("detach signal");
    let connect_manager = manager.clone();
    let mut connect = tokio::spawn(async move {
        connect_manager
            .connect_machine(machine_id, "gaggimate.local")
            .await
    });
    assert!(
        timeout(Duration::from_millis(500), &mut connect)
            .await
            .is_err(),
        "reconnect must wait until the old server attachment is detached"
    );
    release_detach.send(()).expect("release detach");
    timeout(Duration::from_secs(5), flush)
        .await
        .expect("detach completed")
        .expect("detach task")
        .expect("detach succeeded");
    timeout(Duration::from_secs(5), connect)
        .await
        .expect("reconnect completed")
        .expect("reconnect task")
        .expect("reconnect succeeded");
    timeout(Duration::from_secs(5), server)
        .await
        .expect("server completed")
        .expect("server task");

    let machine = registry.machines().expect("machine registry").remove(0);
    assert!(machine.active);
    assert!(!machine.pending_detach);
    assert_eq!(machine.device_id.as_deref(), Some("new-device"));
    let requests = observed.lock().expect("request log");
    assert!(requests[0].starts_with("DELETE "));
    assert!(requests[1].starts_with("GET "));
    assert!(requests[2].starts_with("POST "));
}

#[tokio::test]
async fn account_disconnect_waits_for_in_flight_machine_work() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let registry = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).expect("store"));
    let credentials: Arc<dyn CredentialStore> =
        Arc::new(TestCredentials(StdMutex::new(Some(OAuthTokens {
            access_token: "valid".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        }))));
    let manager = Arc::new(
        MachineManager::open(directory.path(), registry, credentials.clone()).expect("manager"),
    );
    let in_flight = manager.account_gate.read().await;
    let disconnect_manager = manager.clone();
    let mut disconnect = tokio::spawn(async move { disconnect_manager.disconnect_all().await });
    assert!(timeout(Duration::from_millis(20), &mut disconnect)
        .await
        .is_err());
    assert!(credentials.tokens().expect("tokens").is_some());
    drop(in_flight);
    let result = timeout(Duration::from_secs(1), disconnect)
        .await
        .expect("disconnect finished")
        .expect("disconnect task")
        .expect("disconnect succeeded");
    assert_eq!(result["credentialsRemoved"], true);
    assert!(credentials.tokens().expect("tokens").is_none());
}
