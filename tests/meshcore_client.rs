use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use meshcore_mautrix::meshcore::MeshCoreClient;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Seen {
    auth: Vec<String>,
    sends: Vec<Value>,
}

async fn health() -> Json<Value> {
    Json(json!({
        "schema": "meshcore.health.v1",
        "device_id": "dev",
        "connected": true,
        "public_key": "aa",
        "name": "Radioman-Pi",
        "contacts": 3,
        "channels": 8,
        "radio": {"freq_mhz": 910.525, "bw_khz": 62.5, "sf": 7, "cr": 5, "tx_power": 22}
    }))
}

async fn messages(headers: HeaderMap, State(seen): State<Arc<Mutex<Seen>>>) -> Json<Value> {
    seen.lock().unwrap().auth.push(
        headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .into(),
    );
    Json(json!({"messages": [{
        "id": 7,
        "message_id": "mc:7",
        "direction": "inbound",
        "channel": 0,
        "contact": null,
        "sender_name": "Dad",
        "text": "hello",
        "received_at": 1
    }]}))
}

async fn send(
    headers: HeaderMap,
    State(seen): State<Arc<Mutex<Seen>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Bearer test-token") {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad token"})));
    }
    seen.lock().unwrap().sends.push(body);
    (
        StatusCode::ACCEPTED,
        Json(json!({"request_id":"r1","state":"queued"})),
    )
}

#[tokio::test]
async fn client_authenticates_and_maps_wire_types() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/messages", get(messages))
        .route("/v1/send", post(send))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client =
        MeshCoreClient::new(format!("http://{address}").parse().unwrap(), "test-token").unwrap();
    let health = client.health().await.unwrap();
    assert!(health.connected && health.contacts == 3);
    let messages = client.messages(0, 10).await.unwrap();
    assert_eq!(messages[0].text, "hello");
    assert_eq!(messages[0].conversation().unwrap().key(), "channel:0");
    let ack = client.send_channel(0, "hi").await.unwrap();
    assert_eq!(ack.request_id, "r1");
    assert_eq!(
        seen.lock().unwrap().auth[0],
        "Bearer test-token",
        "the sidecar token must be sent"
    );
    assert_eq!(seen.lock().unwrap().sends[0]["channel"], 0);
}
