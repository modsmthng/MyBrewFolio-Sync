use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_tungstenite::accept_async;

fn settings() -> Value {
    json!({"backflushIntervalDays":14,"backflushIntervalShots":0,
        "descalingIntervalWeeks":6,"descalingIntervalShots":200,
        "lastBackflushTime":1780000000,"lastDescalingTime":1780000000,
        "shotsSinceBackflush":10,"shotsSinceDescaling":60,
        "wifiPassword":"private","mqttPassword":"private","boilerTarget":93})
}

// One connection per step proves writes are not retried and readback follows each write.
async fn server(
    responses: Vec<Value>,
    completion: bool,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, response) in responses.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().await.unwrap();
            if completion && index == 1 {
                let mut socket = accept_async(stream).await.unwrap();
                let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let request: Value = serde_json::from_str(&message).unwrap();
                requests.push(request["tp"].as_str().unwrap().to_string());
                socket
                    .send(Message::Text(
                        json!({"tp":"res:cleaning:backflush:complete",
                    "rid":request["rid"],"success":true})
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            } else {
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0_u8; 4096];
                    let length = stream.read(&mut buffer).await.unwrap();
                    assert!(length > 0);
                    bytes.extend_from_slice(&buffer[..length]);
                    let request = String::from_utf8_lossy(&bytes);
                    if let Some(end) = request.find("\r\n\r\n") {
                        let content_length = request[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")?
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + content_length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let body = response.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        }
        requests
    });
    (address, handle)
}

#[test]
fn sanitizes_only_supported_maintenance_fields_and_preserves_disabled_limits() {
    let snapshot = maintenance_settings(&settings()).unwrap();
    assert_eq!(
        snapshot["tasks"]["backflush"],
        json!({"days":14,"brews":0,"lastCompletedAt":1780000000_u64,"shotsSince":10})
    );
    assert_eq!(snapshot["tasks"]["descaling"]["days"], 42);
    assert!(!snapshot.to_string().contains("private"));
    assert_eq!(
        maintenance_settings(&json!({"boilerTarget":93})).unwrap(),
        json!({"supported":false})
    );
    assert!(maintenance_settings(&json!({"backflushIntervalDays":14})).is_err());
    let mut invalid = settings();
    invalid["shotsSinceBackflush"] = json!(-1);
    assert!(maintenance_settings(&invalid).is_err());
}

#[tokio::test]
async fn settings_write_is_partial_converts_weeks_and_verifies_readback() {
    let before = settings();
    let mut after = before.clone();
    after["descalingIntervalWeeks"] = json!(8);
    after["descalingIntervalShots"] = json!(300);
    let (host, handle) = server(vec![before, json!({}), after], false).await;
    let result = GaggiMateClient::new(&host)
        .unwrap()
        .update_maintenance(
            "maintenance_settings",
            &json!({
        "task":"descaling","expected":{"days":42,"brews":200,"lastCompletedAt":1780000000_u64},
        "rules":{"days":56,"brews":300}}),
        )
        .await
        .unwrap();
    assert_eq!(result["maintenance"]["tasks"]["descaling"]["days"], 56);
    let requests = handle.await.unwrap();
    assert!(requests[0].starts_with("GET /api/settings "));
    assert!(requests[1].starts_with("POST /api/settings "));
    assert_eq!(
        requests[1].split("\r\n\r\n").last().unwrap(),
        "descalingIntervalWeeks=8&descalingIntervalShots=300"
    );
    assert!(requests[2].starts_with("GET /api/settings "));
}

#[tokio::test]
async fn stale_machine_settings_are_rejected_before_any_write() {
    let (host, handle) = server(vec![settings()], false).await;
    assert!(GaggiMateClient::new(&host)
        .unwrap()
        .update_maintenance(
            "maintenance_settings",
            &json!({
        "task":"backflush","expected":{"days":7,"brews":0,"lastCompletedAt":1780000000_u64},
        "rules":{"days":21,"brews":100}})
        )
        .await
        .is_err());
    assert_eq!(handle.await.unwrap().len(), 1);
}

#[tokio::test]
async fn unsuccessful_readback_does_not_report_settings_as_saved() {
    let (host, handle) = server(vec![settings(), json!({}), settings()], false).await;
    assert!(GaggiMateClient::new(&host)
        .unwrap()
        .update_maintenance(
            "maintenance_settings",
            &json!({
        "task":"backflush","expected":{"days":14,"brews":0,"lastCompletedAt":1780000000_u64},
        "rules":{"days":21,"brews":100}})
        )
        .await
        .is_err());
    assert_eq!(handle.await.unwrap().len(), 3);
}

#[tokio::test]
async fn completion_uses_the_firmware_command_and_verifies_the_new_checkpoint() {
    let mut after = settings();
    after["lastBackflushTime"] = json!(1780000100_u64);
    after["shotsSinceBackflush"] = json!(0);
    let (host, handle) = server(vec![settings(), json!({}), after], true).await;
    let result = GaggiMateClient::new(&host)
        .unwrap()
        .update_maintenance(
            "maintenance_complete",
            &json!({
        "task":"backflush","expected":{"days":14,"brews":0,"lastCompletedAt":1780000000_u64}}),
        )
        .await
        .unwrap();
    assert_eq!(result["maintenance"]["tasks"]["backflush"]["shotsSince"], 0);
    assert_eq!(handle.await.unwrap()[1], "req:cleaning:backflush:complete");
}
