use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

use super::super::{profile_install_operation, profile_install_preview_operation};
use crate::{
    local::GaggiMateClient,
    profile_image::{device_image, tests::image_fixture},
    store::BridgeCompletion,
};

#[derive(Default)]
struct Machine {
    profile: Option<Value>,
    saves: usize,
    uploads: Vec<(String, Vec<u8>)>,
    upload_status: u16,
    fail_next_load: bool,
    actions: usize,
}

async fn machine_request(mut stream: TcpStream, state: Arc<Mutex<Machine>>) {
    let mut first = [0; 1];
    stream.peek(&mut first).await.unwrap();
    if first[0] == b'G' {
        let mut socket = accept_async(stream).await.unwrap();
        let request: Value =
            serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap())
                .unwrap();
        let mut response = json!({ "rid": request["rid"], "tp": request["tp"].as_str().unwrap().replacen("req:", "res:", 1) });
        {
            let mut machine = state.lock().unwrap();
            match request["tp"].as_str().unwrap() {
                "req:profiles:list" => {
                    response["profiles"] = machine
                        .profile
                        .iter()
                        .map(|profile| json!({"id": profile["id"], "label": profile["label"]}))
                        .collect()
                }
                "req:profiles:load" if machine.fail_next_load => {
                    machine.fail_next_load = false;
                    return;
                }
                "req:profiles:load" => {
                    response["profile"] = machine.profile.clone().unwrap_or(Value::Null)
                }
                "req:profiles:save" => {
                    assert!(
                        request["profile"].get("image").is_none(),
                        "PNG must never be sent over WebSocket"
                    );
                    machine.saves += 1;
                    let mut saved = request["profile"].clone();
                    if machine.profile.is_none() {
                        saved["id"] = json!("machine-assigned");
                    }
                    machine.profile = Some(saved.clone());
                    response["profile"] = saved;
                }
                "req:profiles:favorite" | "req:profiles:select" => machine.actions += 1,
                other => panic!("Unexpected request {other}"),
            }
        }
        socket
            .send(Message::Text(response.to_string().into()))
            .await
            .unwrap();
    } else {
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        let header_end = loop {
            let size = stream.read(&mut buf).await.unwrap();
            assert!(size > 0);
            request.extend_from_slice(&buf[..size]);
            if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let target = headers.lines().next().unwrap().to_string();
        assert!(target.starts_with("POST /api/profiles/image?id="));
        assert!(headers
            .to_lowercase()
            .contains("content-type: application/octet-stream"));
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_string)
            })
            .unwrap()
            .parse()
            .unwrap();
        while request.len() < header_end + length {
            let size = stream.read(&mut buf).await.unwrap();
            assert!(size > 0);
            request.extend_from_slice(&buf[..size]);
        }
        let status = {
            let mut machine = state.lock().unwrap();
            machine
                .uploads
                .push((target, request[header_end..].to_vec()));
            machine.upload_status
        };
        let body = if status == 404 { "No SD card" } else { "{}" };
        let content_type = if status == 404 {
            "text/plain"
        } else {
            "application/json"
        };
        stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    }
}

async fn machine(
    profile: Option<Value>,
    upload_status: u16,
    fail_next_load: bool,
) -> (GaggiMateClient, Arc<Mutex<Machine>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(Machine {
        profile,
        upload_status,
        fail_next_load,
        ..Machine::default()
    }));
    let server = state.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(machine_request(stream, server.clone()));
        }
    });
    (GaggiMateClient::new(&address.to_string()).unwrap(), state)
}

fn profile() -> Value {
    json!({"id": "store-profile", "label": "Image test", "type": "pro", "phases": [{"name": "Brew", "duration": 20}]})
}

fn payload() -> Value {
    let mut profile = profile();
    profile["image"] = image_fixture();
    json!({"profile": profile, "expectedCollision": "none"})
}

#[tokio::test]
async fn saves_core_then_uploads_exact_rgb565_with_the_assigned_id() {
    let (local, state) = machine(None, 200, false).await;
    let payload = payload();
    let preview = profile_install_preview_operation(&local, &payload)
        .await
        .unwrap();
    assert_eq!(preview["collision"], "none");
    assert_eq!(state.lock().unwrap().saves, 0);
    let installed = profile_install_operation(&local, &payload).await.unwrap();
    assert_eq!(installed["installed"], true);
    assert_eq!(installed["profileId"], "machine-assigned");
    let state = state.lock().unwrap();
    assert_eq!(state.saves, 1);
    assert!(state.profile.as_ref().unwrap().get("image").is_none());
    assert_eq!(state.uploads.len(), 1);
    assert_eq!(
        state.uploads[0].0,
        "POST /api/profiles/image?id=machine-assigned HTTP/1.1"
    );
    assert_eq!(
        state.uploads[0].1,
        device_image(payload["profile"].get("image"))
            .unwrap()
            .unwrap()
    );
}

#[tokio::test]
async fn identical_core_with_a_new_image_requires_overwrite_confirmation() {
    let (local, state) = machine(Some(profile()), 200, false).await;
    let mut payload = payload();
    assert_eq!(
        profile_install_preview_operation(&local, &payload)
            .await
            .unwrap()["collision"],
        "different"
    );
    assert_eq!(
        profile_install_operation(&local, &payload)
            .await
            .unwrap_err()
            .0,
        "PROFILE_CHANGED"
    );
    assert_eq!(state.lock().unwrap().saves, 0);
    payload["expectedCollision"] = json!("different");
    assert_eq!(
        profile_install_operation(&local, &payload).await.unwrap()["alreadyInstalled"],
        false
    );
    assert_eq!(state.lock().unwrap().uploads.len(), 1);
}

#[tokio::test]
async fn no_sd_card_keeps_the_profile_install_successful() {
    let (local, state) = machine(None, 404, false).await;
    assert_eq!(
        profile_install_operation(&local, &payload()).await.unwrap()["installed"],
        true
    );
    assert_eq!(state.lock().unwrap().saves, 1);
}

#[tokio::test]
async fn plain_profiles_need_no_image_endpoint() {
    let (local, state) = machine(None, 500, false).await;
    let payload = json!({"profile": profile(), "expectedCollision": "none"});
    assert_eq!(
        profile_install_operation(&local, &payload).await.unwrap()["installed"],
        true
    );
    assert!(state.lock().unwrap().uploads.is_empty());
}

#[tokio::test]
async fn invalid_image_fails_before_saving_and_upload_errors_are_explicit() {
    let (local, state) = machine(None, 500, false).await;
    let mut invalid = payload();
    invalid["profile"]["image"] = json!("invalid PNG");
    assert_eq!(
        profile_install_operation(&local, &invalid)
            .await
            .unwrap_err()
            .0,
        "PROFILE_IMAGE_INVALID"
    );
    assert_eq!(state.lock().unwrap().saves, 0);
    let mut payload = payload();
    payload["favorite"] = json!(true);
    assert_eq!(
        profile_install_operation(&local, &payload)
            .await
            .unwrap_err()
            .0,
        "PROFILE_IMAGE_UPLOAD_FAILED"
    );
    assert_eq!(state.lock().unwrap().saves, 1);
    assert_eq!(state.lock().unwrap().actions, 0);
}

#[tokio::test]
async fn resumed_confirmation_uploads_the_frozen_image_without_saving_the_profile_again() {
    let (local, state) = machine(None, 200, true).await;
    let payload = payload();
    let pending = profile_install_operation(&local, &payload)
        .await
        .unwrap_err();
    assert_eq!(
        pending,
        ("SAVE_CONFIRMATION_PENDING", "machine-assigned".into())
    );
    assert!(state.lock().unwrap().uploads.is_empty());
    let (engine, _directory) = super::test_engine();
    let completion = BridgeCompletion {
        operation_id: "image-install".into(),
        lease_token: "image-lease".into(),
        payload: json!({"profileStoreConfirmationPending": true, "profileId": pending.1, "image": payload["profile"]["image"]}),
    };
    engine
        .store
        .queue_bridge_completion(
            &completion.operation_id,
            &completion.lease_token,
            &completion.payload,
        )
        .unwrap();
    let result = engine
        .profile_store_completion_payload(&local, &completion)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "completed");
    let saved = engine.store.bridge_completions(1).unwrap().remove(0);
    assert_eq!(
        engine
            .profile_store_completion_payload(&local, &saved)
            .await
            .unwrap()
            .unwrap(),
        result
    );
    assert_eq!(state.lock().unwrap().saves, 1);
    assert_eq!(state.lock().unwrap().uploads.len(), 1);
}
