// SPDX-License-Identifier: GPL-3.0-or-later
use super::*;
use axum::{body::to_bytes, http::Request as HttpRequest, Extension};
use mybrewfolio_sync_lib::{
    credentials::{CredentialStore, EncryptedFileCredentialStore},
    model::{OAuthTokens, SyncObject},
    store::{AppStore, MachineRecord},
};
use tower::ServiceExt;

fn fixture() -> (WebState, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let key = EncryptedFileCredentialStore::initialize_key(directory.path()).unwrap();
    let credentials = Arc::new(
        EncryptedFileCredentialStore::from_key_file(directory.path().join("credentials.enc"), &key)
            .unwrap(),
    );
    let store = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).unwrap());
    let manager = Arc::new(MachineManager::open(directory.path(), store, credentials).unwrap());
    let (_, setup) = watch::channel(json!({"state": "machine_selection"}));
    (
        WebState {
            manager,
            setup,
            root: directory.path().into(),
            csrf: "private-csrf".into(),
        },
        directory,
    )
}

async fn send(
    state: WebState,
    peer: &str,
    method: &str,
    path: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> Response {
    let router = router(state).layer(Extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap())));
    let mut request = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    router
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn only_the_supervisor_tcp_peer_can_read_even_with_forged_headers() {
    let (state, _directory) = fixture();
    for peer in ["127.0.0.1:1", "172.30.32.3:1", "192.168.1.2:1"] {
        assert_eq!(
            send(
                state.clone(),
                peer,
                "GET",
                "/api/status",
                Value::Null,
                &[
                    ("x-forwarded-for", "172.30.32.2"),
                    ("x-remote-user-id", "admin")
                ]
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    let response = send(
        state,
        "172.30.32.2:1",
        "GET",
        "/api/status",
        Value::Null,
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["csrfToken"], "private-csrf");
    assert_eq!(value["status"]["connected"], false);
    assert!(value.get("tokens").is_none());
}

#[tokio::test]
async fn csrf_and_explicit_confirmation_are_both_required() {
    let (state, _directory) = fixture();
    for headers in [
        vec![],
        vec![("x-mybrewfolio-csrf", "wrong")],
        vec![
            ("x-mybrewfolio-csrf", "private-csrf"),
            ("sec-fetch-site", "cross-site"),
        ],
        vec![("x-mybrewfolio-csrf", "private-csrf"), ("origin", "null")],
    ] {
        assert_eq!(
            send(
                state.clone(),
                "172.30.32.2:1",
                "POST",
                "/api/action",
                json!({"action":"disconnect", "confirm":true}),
                &headers
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        send(
            state,
            "172.30.32.2:1",
            "POST",
            "/api/action",
            json!({"action":"disconnect", "confirm":false}),
            &[("x-mybrewfolio-csrf", "private-csrf")]
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn the_web_adapter_cannot_dispatch_arbitrary_cli_or_notes_writes() {
    let (state, _directory) = fixture();
    for body in [
        json!({"action":"notes", "args":["restore"]}),
        json!({"action":"connect", "command":"resync"}),
    ] {
        assert_eq!(
            send(
                state.clone(),
                "172.30.32.2:1",
                "POST",
                "/api/action",
                body,
                &[("x-mybrewfolio-csrf", "private-csrf")]
            )
            .await
            .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}

#[tokio::test]
async fn assets_are_relative_safe_and_do_not_expose_state_files() {
    let (state, directory) = fixture();
    std::fs::write(
        directory.path().join("index.html"),
        "<script src=\"./assets/app.js\"></script>",
    )
    .unwrap();
    std::fs::create_dir(directory.path().join("assets")).unwrap();
    std::fs::write(
        directory.path().join("assets/app.js"),
        "console.log('ready')",
    )
    .unwrap();
    for path in ["/", "/assets/app.js"] {
        assert_eq!(
            send(
                state.clone(),
                "172.30.32.2:1",
                "GET",
                path,
                Value::Null,
                &[]
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    for path in [
        "/sync.sqlite",
        "/state.key",
        "/assets/../state.key",
        "/assets/%2e%2e/state.key",
        "/api/unknown",
    ] {
        assert_eq!(
            send(
                state.clone(),
                "172.30.32.2:1",
                "GET",
                path,
                Value::Null,
                &[]
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn state_restores_machine_addresses_credentials_and_queue_with_the_original_key() {
    let directory = tempfile::tempdir().unwrap();
    let key = EncryptedFileCredentialStore::initialize_key(directory.path()).unwrap();
    let credentials = Arc::new(
        EncryptedFileCredentialStore::from_key_file(directory.path().join("credentials.enc"), &key)
            .unwrap(),
    );
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "private-test-access".into(),
            refresh_token: Some("private-test-refresh".into()),
            expires_at: i64::MAX,
        })
        .unwrap();
    let store = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).unwrap());
    let id = "11111111-1111-4111-8111-111111111111";
    store.set_setting("machine_host", "192.168.1.42").unwrap();
    store.set_setting("device_id", "test-device").unwrap();
    store.set_setting("source_id", id).unwrap();
    store
        .save_machine(&MachineRecord {
            id: id.into(),
            name: "Kitchen".into(),
            device_id: Some("test-device".into()),
            active: true,
            pending_detach: false,
            legacy_default: true,
        })
        .unwrap();
    store
        .queue(&SyncObject {
            kind: "profile".into(),
            source_key: "test-profile".into(),
            source_hash: "hash".into(),
            shot_source_key: None,
            data: json!({"name":"Saved"}),
        })
        .unwrap();
    let before = MachineManager::open(directory.path(), store.clone(), credentials.clone())
        .unwrap()
        .status_json()
        .await;
    drop(store);
    let reopened_credentials = Arc::new(
        EncryptedFileCredentialStore::from_key_file(directory.path().join("credentials.enc"), &key)
            .unwrap(),
    );
    assert_eq!(
        reopened_credentials
            .tokens()
            .unwrap()
            .unwrap()
            .refresh_token
            .as_deref(),
        Some("private-test-refresh")
    );
    let reopened = MachineManager::open(
        directory.path(),
        Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).unwrap()),
        reopened_credentials,
    )
    .unwrap();
    assert_eq!(
        AppStore::open(&directory.path().join("sync.sqlite"))
            .unwrap()
            .pending_count()
            .unwrap(),
        1
    );
    let after = reopened.status_json().await;
    assert_eq!(
        after["machines"][0]["machineId"],
        before["machines"][0]["machineId"]
    );
    assert_eq!(after["machines"][0]["machineHost"], "192.168.1.42");
    assert_eq!(after["connected"], true);
    assert!(!after.to_string().contains("private-test"));
}

#[tokio::test]
async fn address_changes_are_limited_to_the_selected_connected_machine() {
    let (mut state, directory) = fixture();
    let key = directory.path().join("state.key");
    let credentials = Arc::new(
        EncryptedFileCredentialStore::from_key_file(directory.path().join("credentials.enc"), &key)
            .unwrap(),
    );
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "private-test-access".into(),
            refresh_token: None,
            expires_at: i64::MAX,
        })
        .unwrap();
    let store = Arc::new(AppStore::open(&directory.path().join("sync.sqlite")).unwrap());
    let kitchen = "11111111-1111-4111-8111-111111111111";
    let office = "22222222-2222-4222-8222-222222222222";
    store.set_setting("machine_host", "192.168.1.41").unwrap();
    for (id, name, legacy_default) in [(kitchen, "Kitchen", true), (office, "Office", false)] {
        store
            .save_machine(&MachineRecord {
                id: id.into(),
                name: name.into(),
                device_id: Some(format!("device-{name}")),
                active: true,
                pending_detach: false,
                legacy_default,
            })
            .unwrap();
    }
    state.manager = Arc::new(MachineManager::open(directory.path(), store, credentials).unwrap());
    let response = send(
        state.clone(),
        "172.30.32.2:1",
        "POST",
        "/api/action",
        json!({"action":"setHost", "machineId": office, "host":"192.168.1.42"}),
        &[("x-mybrewfolio-csrf", "private-csrf")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        state
            .manager
            .engine(kitchen)
            .await
            .unwrap()
            .status()
            .await
            .machine_host,
        "192.168.1.41"
    );
    assert_eq!(
        state
            .manager
            .engine(office)
            .await
            .unwrap()
            .status()
            .await
            .machine_host,
        "192.168.1.42"
    );
    for host in ["8.8.8.8", "https://evil.test", "192.168.1.1/private"] {
        assert_eq!(
            send(
                state.clone(),
                "172.30.32.2:1",
                "POST",
                "/api/action",
                json!({"action":"setHost", "machineId": office, "host":host}),
                &[("x-mybrewfolio-csrf", "private-csrf")]
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        state
            .manager
            .engine(office)
            .await
            .unwrap()
            .status()
            .await
            .machine_host,
        "192.168.1.42"
    );
}
