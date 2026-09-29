// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::{CloudClient, CloudConfig, CloudError, CredentialStore, OAuthTokens};
use crate::{
    model::{SyncProgress, SyncProgressPhase},
    store::StoreError,
};

#[derive(Default)]
struct TestCredentials {
    tokens: Mutex<Option<OAuthTokens>>,
    pending: Mutex<Option<String>>,
}

impl CredentialStore for TestCredentials {
    fn save_tokens(&self, tokens: &OAuthTokens) -> Result<(), StoreError> {
        *self
            .tokens
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)? = Some(tokens.clone());
        Ok(())
    }

    fn tokens(&self) -> Result<Option<OAuthTokens>, StoreError> {
        Ok(self
            .tokens
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?
            .clone())
    }

    fn delete_tokens(&self) -> Result<(), StoreError> {
        *self
            .tokens
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)? = None;
        Ok(())
    }

    fn save_pending_device_authorization(&self, value: &str) -> Result<(), StoreError> {
        *self
            .pending
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)? = Some(value.into());
        Ok(())
    }

    fn pending_device_authorization(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .pending
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)?
            .clone())
    }

    fn delete_pending_device_authorization(&self) -> Result<(), StoreError> {
        *self
            .pending
            .lock()
            .map_err(|_| StoreError::InvalidCredentials)? = None;
        Ok(())
    }
}

async fn response_server(responses: Vec<(&str, &str)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let address = listener.local_addr().expect("test address");
    let responses = responses
        .into_iter()
        .map(|(status, body)| (status.to_string(), body.to_string()))
        .collect::<Vec<_>>();
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().await.expect("request accepted");
            let mut input = [0_u8; 8_192];
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut input)).await;
            let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response written");
        }
    });
    format!("http://{address}")
}

async fn contract_server(routes: Vec<(&str, &str, &str)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let address = listener.local_addr().expect("test address");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_requests = observed.clone();
    let routes = routes
        .into_iter()
        .map(|(request, status, body)| (request.to_string(), status.to_string(), body.to_string()))
        .collect::<Vec<_>>();
    tokio::spawn(async move {
        for (expected, status, body) in routes {
            let (mut stream, _) = listener.accept().await.expect("request accepted");
            let mut input = [0_u8; 8_192];
            let length = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut input))
                .await
                .expect("request arrives")
                .expect("request read");
            let request = String::from_utf8_lossy(&input[..length]).into_owned();
            let request_line = request.lines().next().unwrap_or_default().to_string();
            observed_requests
                .lock()
                .expect("observed requests")
                .push(request);
            let (status, body) = if request_line == expected {
                (status, body)
            } else {
                ("500 Internal Server Error".to_string(), "{}".to_string())
            };
            let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response written");
        }
    });
    (format!("http://{address}"), observed)
}

fn client(credentials: Arc<TestCredentials>, url: &str) -> CloudClient {
    let mut client = CloudClient::new(credentials).expect("cloud client");
    client.config = CloudConfig {
        api_url: url.into(),
        client_id: "test-client".into(),
        authorize_url: "https://login.example.test/authorize".into(),
        token_url: url.into(),
        redirect_uri: "mybrewfolio-sync://oauth/callback".into(),
        device_redirect_uri: "https://mybrewfolio.example.test/v1/sync/device-auth/callback".into(),
    };
    client
}

#[tokio::test]
async fn full_sync_slots_allow_only_two_concurrent_runs() {
    let client = client(Arc::new(TestCredentials::default()), "http://127.0.0.1:1");
    let first = client.full_sync_slot().await;
    let second = client.full_sync_slot().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), client.full_sync_slot())
            .await
            .is_err()
    );
    drop(first);
    let third = tokio::time::timeout(Duration::from_secs(1), client.full_sync_slot())
        .await
        .expect("slot released");
    drop(second);
    drop(third);
}

#[test]
fn authorization_uses_pkce_and_a_random_state() {
    let credentials = Arc::new(TestCredentials::default());
    let client = client(credentials, "http://127.0.0.1:1");

    let (url, pending) = client.authorization().expect("authorization URL");

    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("login.example.test"));
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "code_challenge_method")
            .map(|(_, value)| value),
        Some("S256".into())
    );
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value),
        Some(pending.state.into())
    );
    assert!(!pending.verifier.is_empty());
}

#[tokio::test]
async fn device_pairing_exchanges_the_one_time_code_without_server_token_storage() {
    let url = response_server(vec![
            ("201 Created", r#"{"requestId":"request-1","userCode":"ABCD-1234","verificationUri":"https://mybrewfolio.example.test/pair","pollToken":"poll-secret","expiresIn":600}"#),
            ("200 OK", r#"{"status":"authorized","authorizationCode":"one-time-code"}"#),
            ("200 OK", r#"{"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600}"#),
        ]).await;
    let credentials = Arc::new(TestCredentials::default());
    let client = client(credentials.clone(), &url);

    let (info, pending) = client
        .begin_device_authorization()
        .await
        .expect("pairing begins");
    assert_eq!(info.user_code, "ABCD-1234");
    assert_eq!(info.expires_in, 600);
    assert_eq!(
        client
            .poll_device_authorization(&pending)
            .await
            .expect("pairing polls"),
        Some(())
    );

    assert_eq!(
        credentials
            .tokens()
            .expect("tokens stored")
            .expect("tokens present")
            .access_token,
        "access-token"
    );
}

#[tokio::test]
async fn rejected_device_pairing_is_reported_without_exchanging_a_token() {
    let url = response_server(vec![
            ("201 Created", r#"{"requestId":"request-1","userCode":"ABCD-1234","verificationUri":"https://mybrewfolio.example.test/pair","pollToken":"poll-secret","expiresIn":600}"#),
            ("409 Conflict", r#"{"error":"Device authorization was rejected or already consumed"}"#),
        ])
        .await;
    let credentials = Arc::new(TestCredentials::default());
    let client = client(credentials.clone(), &url);

    let (_, pending) = client
        .begin_device_authorization()
        .await
        .expect("pairing begins");
    let error = client
        .poll_device_authorization(&pending)
        .await
        .expect_err("rejected pairing is terminal");

    assert!(matches!(error, CloudError::DeviceAuthorizationRejected));
    assert!(credentials.tokens().expect("tokens read").is_none());
}

#[tokio::test]
async fn failed_device_code_exchange_can_start_a_fresh_pairing_attempt() {
    let url = response_server(vec![
            ("201 Created", r#"{"requestId":"request-1","userCode":"ABCD-1234","verificationUri":"https://mybrewfolio.example.test/pair","pollToken":"poll-secret","expiresIn":600}"#),
            ("200 OK", r#"{"status":"authorized","authorizationCode":"one-time-code"}"#),
            ("400 Bad Request", r#"{"error":"invalid authorization code"}"#),
        ])
        .await;
    let credentials = Arc::new(TestCredentials::default());
    let client = client(credentials.clone(), &url);

    let (_, pending) = client
        .begin_device_authorization()
        .await
        .expect("pairing begins");
    let error = client
        .poll_device_authorization(&pending)
        .await
        .expect_err("failed exchange requires a fresh pairing attempt");

    assert!(matches!(
        error,
        CloudError::DeviceAuthorizationExchangeFailed
    ));
    assert!(credentials.tokens().expect("tokens read").is_none());
}

#[tokio::test]
async fn expired_tokens_refresh_before_an_authorized_state_request() {
    let url = response_server(vec![
        (
            "200 OK",
            r#"{"access_token":"new-access","expires_in":3600}"#,
        ),
        (
            "200 OK",
            r#"{"items":[],"source":{"duplicatePolicy":"reuse_matching"}}"#,
        ),
    ])
    .await;
    let credentials = Arc::new(TestCredentials::default());
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "expired-access".into(),
            refresh_token: Some("refresh-token".into()),
            expires_at: 0,
        })
        .expect("expired token stored");
    let client = client(credentials.clone(), &url);

    let state = client.state("device-1").await.expect("state loaded");

    assert_eq!(state["source"]["duplicatePolicy"], "reuse_matching");
    assert_eq!(
        credentials
            .tokens()
            .expect("tokens read")
            .expect("token present")
            .access_token,
        "new-access"
    );
}

#[tokio::test]
async fn simultaneous_requests_share_one_token_refresh() {
    let (url, observed) = contract_server(vec![
        (
            "POST / HTTP/1.1",
            "200 OK",
            r#"{"access_token":"new-access","expires_in":3600}"#,
        ),
        ("GET /v1/sync/state HTTP/1.1", "200 OK", r#"{"items":[]}"#),
        ("GET /v1/sync/state HTTP/1.1", "200 OK", r#"{"items":[]}"#),
    ])
    .await;
    let credentials = Arc::new(TestCredentials::default());
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "expired".into(),
            refresh_token: Some("refresh".into()),
            expires_at: 0,
        })
        .expect("tokens stored");
    let client = client(credentials, &url);
    let (first, second) = tokio::join!(client.state("device-1"), client.state("device-1"));
    assert!(first.is_ok() && second.is_ok());
    let requests = observed.lock().expect("requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("POST /"))
            .count(),
        1
    );
}

#[tokio::test]
async fn stale_access_token_refreshes_and_retries_once() {
    let (url, observed) = contract_server(vec![
        (
            "GET /v1/sync/state HTTP/1.1",
            "401 Unauthorized",
            r#"{"details":{"code":"SYNC_ACCESS_TOKEN_INVALID"}}"#,
        ),
        (
            "POST / HTTP/1.1",
            "200 OK",
            r#"{"access_token":"new-access","expires_in":3600}"#,
        ),
        ("GET /v1/sync/state HTTP/1.1", "200 OK", r#"{"items":[]}"#),
    ])
    .await;
    let credentials = Arc::new(TestCredentials::default());
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "stale".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        })
        .expect("tokens stored");
    let client = client(credentials, &url);
    assert!(client.state("device-1").await.is_ok());
    let requests = observed.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[2].contains("Bearer new-access"));
}

#[tokio::test]
async fn revoked_device_does_not_trigger_a_refresh() {
    let url = response_server(vec![(
        "401 Unauthorized",
        r#"{"details":{"code":"SYNC_DEVICE_REVOKED"}}"#,
    )])
    .await;
    let credentials = Arc::new(TestCredentials::default());
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "valid".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        })
        .expect("tokens stored");
    let client = client(credentials, &url);
    assert!(matches!(
        client.state("device-1").await,
        Err(CloudError::Revoked)
    ));
}

#[tokio::test]
async fn unknown_unauthorized_response_keeps_the_installation_credentials() {
    let url = response_server(vec![("401 Unauthorized", r#"{"error":"unknown"}"#)]).await;
    let credentials = Arc::new(TestCredentials::default());
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "valid".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        })
        .expect("tokens stored");
    let client = client(credentials.clone(), &url);
    assert!(matches!(
        client.state("device-1").await,
        Err(CloudError::AuthenticationRejected)
    ));
    assert!(credentials.tokens().expect("tokens read").is_some());
}

#[tokio::test]
async fn refresh_errors_distinguish_reauthentication_from_transient_failures() {
    for (status, body, reauth_required) in [
        ("400 Bad Request", r#"{"error":"invalid_grant"}"#, true),
        (
            "429 Too Many Requests",
            r#"{"error":"rate_limited"}"#,
            false,
        ),
        (
            "503 Service Unavailable",
            r#"{"error":"temporarily_unavailable"}"#,
            false,
        ),
    ] {
        let url = response_server(vec![(status, body)]).await;
        let credentials = Arc::new(TestCredentials::default());
        credentials
            .save_tokens(&OAuthTokens {
                access_token: "expired".into(),
                refresh_token: Some("refresh".into()),
                expires_at: 0,
            })
            .expect("tokens stored");
        let client = client(credentials.clone(), &url);
        let result = client.state("device-1").await;
        assert_eq!(
            matches!(result, Err(CloudError::ReauthRequired)),
            reauth_required,
            "{status}"
        );
        assert!(credentials.tokens().expect("tokens read").is_some());
    }

    let credentials = Arc::new(TestCredentials::default());
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "expired".into(),
            refresh_token: None,
            expires_at: 0,
        })
        .expect("tokens stored");
    let client = client(credentials.clone(), "http://127.0.0.1:1");
    assert!(matches!(
        client.state("device-1").await,
        Err(CloudError::ReauthRequired)
    ));
    assert!(credentials.tokens().expect("tokens read").is_some());

    credentials
        .save_tokens(&OAuthTokens {
            access_token: "expired".into(),
            refresh_token: Some("refresh".into()),
            expires_at: 0,
        })
        .expect("tokens stored");
    assert!(matches!(
        client.state("device-1").await,
        Err(CloudError::Unreachable)
    ));
    assert!(credentials.tokens().expect("tokens read").is_some());
}

#[tokio::test]
async fn cloud_operations_use_the_documented_authenticated_endpoints() {
    let (url, observed) = contract_server(vec![
        (
            "POST /v1/sync/devices HTTP/1.1",
            "200 OK",
            r#"{"device":{"id":"device-1","sourceId":"source-1"}}"#,
        ),
        (
            "GET /v1/sync/state HTTP/1.1",
            "200 OK",
            r#"{"items":[],"source":{}}"#,
        ),
        (
            "POST /v1/sync/batches HTTP/1.1",
            "200 OK",
            r#"{"results":[{"index":0,"status":"created"}]}"#,
        ),
        (
            "POST /v1/sync/notes/two-way/request HTTP/1.1",
            "200 OK",
            "{}",
        ),
        ("DELETE /v1/sync/notes/two-way HTTP/1.1", "200 OK", "{}"),
        (
            "POST /v1/sync/notes/backups HTTP/1.1",
            "200 OK",
            r#"{"backup":{"id":"backup-1"}}"#,
        ),
        (
            "POST /v1/sync/notes/backups/backup-1/items HTTP/1.1",
            "200 OK",
            "{}",
        ),
        (
            "POST /v1/sync/notes/backups/backup-1/finalize HTTP/1.1",
            "200 OK",
            "{}",
        ),
        (
            "GET /v1/sync/notes/activation-preview/backup-1 HTTP/1.1",
            "200 OK",
            r#"{"items":[]}"#,
        ),
        (
            "POST /v1/sync/notes/two-way/activate HTTP/1.1",
            "200 OK",
            "{}",
        ),
        (
            "POST /v1/sync/notes/outbound/claim HTTP/1.1",
            "200 OK",
            r#"{"operations":[]}"#,
        ),
        (
            "POST /v1/sync/notes/outbound/op-1/result HTTP/1.1",
            "200 OK",
            "{}",
        ),
        (
            "GET /v1/sync/notes/backups/backup-1/items HTTP/1.1",
            "200 OK",
            r#"{"items":[]}"#,
        ),
        (
            "POST /v1/sync/notes/backups/backup-1/restore-results HTTP/1.1",
            "200 OK",
            r#"{"applied":1}"#,
        ),
        ("PUT /v1/sync/settings HTTP/1.1", "200 OK", "{}"),
        (
            "POST /v1/sync/resync/preview HTTP/1.1",
            "200 OK",
            r#"{"restoreItems":[]}"#,
        ),
        (
            "POST /v1/sync/resync/apply HTTP/1.1",
            "200 OK",
            r#"{"restored":0}"#,
        ),
        ("POST /v1/sync/heartbeat HTTP/1.1", "200 OK", "{}"),
        ("DELETE /v1/sync/devices/device-1 HTTP/1.1", "200 OK", "{}"),
    ])
    .await;
    let credentials = Arc::new(TestCredentials::default());
    credentials
        .save_tokens(&OAuthTokens {
            access_token: "valid-access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
        })
        .expect("token stored");
    let client = client(credentials, &url);
    let item = crate::model::SyncObject {
        kind: "shot".into(),
        source_key: "1:2".into(),
        source_hash: "hash".into(),
        shot_source_key: None,
        data: json!({ "id": "1" }),
    };

    let registered = client
        .register_device("installation-1", "Test machine", "linux", "0.3.12")
        .await
        .expect("device registered");
    assert_eq!(registered.id, "device-1");
    client.state("device-1").await.expect("state loaded");
    client
        .batch("device-1", &[item])
        .await
        .expect("batch accepted");
    client
        .request_two_way_notes("device-1")
        .await
        .expect("two-way Notes requested");
    client
        .disable_two_way_notes("device-1")
        .await
        .expect("two-way Notes disabled");
    client
        .begin_notes_backup("device-1", "latest")
        .await
        .expect("backup started");
    client
        .add_notes_backup_items("device-1", "backup-1", &[json!({ "sourceKey": "1:2" })])
        .await
        .expect("backup items added");
    client
        .finalize_notes_backup("device-1", "backup-1", "inventory-hash")
        .await
        .expect("backup finalized");
    client
        .notes_activation_preview("device-1", "backup-1")
        .await
        .expect("activation preview loaded");
    client
        .activate_two_way_notes("device-1", "backup-1", json!({ "items": [] }))
        .await
        .expect("two-way Notes activated");
    client
        .claim_outbound_notes("device-1")
        .await
        .expect("outbound Notes claimed");
    client
        .complete_outbound_note("device-1", "op-1", json!({ "status": "applied" }))
        .await
        .expect("outbound Notes completed");
    client
        .notes_backup_items("device-1", "backup-1")
        .await
        .expect("backup items loaded");
    client
        .apply_notes_restore_results("device-1", "backup-1", json!([]))
        .await
        .expect("restore result applied");
    client
        .save_settings("device-1", "reuse_matching")
        .await
        .expect("settings saved");
    client
        .resync_preview("device-1", json!([]))
        .await
        .expect("resync preview loaded");
    client
        .resync_apply("device-1", json!({ "restoreItemIds": [] }))
        .await
        .expect("resync applied");
    client
        .heartbeat(
            "device-1",
            true,
            Some("2026-08-27T10:00:00.000Z"),
            None,
            Some(&SyncProgress {
                phase: SyncProgressPhase::Uploading,
                scanned_shots: 4,
                total_shots: 4,
                uploaded_items: Some(25),
                total_items: Some(50),
            }),
        )
        .await
        .expect("heartbeat sent");
    client.revoke("device-1").await.expect("device revoked");

    let requests = observed.lock().expect("observed requests");
    assert_eq!(requests.len(), 19);
    assert!(requests.iter().all(
        |request| request.contains("authorization: Bearer valid-access")
            || request.contains("Authorization: Bearer valid-access")
    ));
    assert!(requests[0].contains("installationId"));
    assert!(
        requests
            .iter()
            .filter(|request| request.contains("canonicalNotesHash"))
            .count()
            >= 2
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.contains("twoWayNotesProtocol"))
            .count()
            >= 2
    );
    assert!(requests[2].contains("sourceKey"));
    assert!(requests[13].contains("items"));
    assert!(requests[14].contains("duplicatePolicy"));
    assert!(requests.iter().any(|request| {
        request.contains("\"syncProgress\":{\"phase\":\"uploading\"")
            && request.contains("\"uploadedItems\":25")
    }));
}
