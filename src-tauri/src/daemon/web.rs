// SPDX-License-Identifier: GPL-3.0-or-later

//! A narrow ingress adapter over the daemon's existing manager. No tokens,
//! arbitrary CLI commands, or machine-write protocols are exposed over HTTP.
use axum::{
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    http::{header, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use mybrewfolio_sync_lib::{local::normalize_host, machines::MachineManager};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};
use tokio::{net::TcpListener, sync::watch};
use uuid::Uuid;

#[derive(Clone)]
struct WebState {
    manager: Arc<MachineManager>,
    setup: watch::Receiver<Value>,
    root: PathBuf,
    csrf: String,
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn api_error(error: impl ToString) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": error.to_string()})),
    )
}

fn is_supervisor(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip == Ipv4Addr::new(172, 30, 32, 2),
        IpAddr::V6(ip) => ip.to_ipv4_mapped() == Some(Ipv4Addr::new(172, 30, 32, 2)),
    }
}

async fn ingress_guard(State(state): State<WebState>, request: Request, next: Next) -> Response {
    // Trust the TCP peer, never caller-controlled forwarding/identity headers.
    let trusted = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| is_supervisor(peer.0.ip()));
    if !trusted {
        return StatusCode::FORBIDDEN.into_response();
    }
    let headers = request.headers();
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value == "cross-site")
        || headers
            .get(header::ORIGIN)
            .is_some_and(|value| value == "null")
        || (request.method() != Method::GET
            && request.method() != Method::HEAD
            && headers
                .get("x-mybrewfolio-csrf")
                .and_then(|value| value.to_str().ok())
                != Some(&state.csrf))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'self'; base-uri 'none'; form-action 'self'; object-src 'none'".parse().unwrap());
    response
}

async fn status(State(state): State<WebState>) -> Json<Value> {
    Json(json!({
        "status": state.manager.status_json().await,
        "setup": state.setup.borrow().clone(),
        "csrfToken": state.csrf,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn machines(State(state): State<WebState>) -> ApiResult {
    state
        .manager
        .list_account_machines()
        .await
        .map(|machines| Json(json!({"machines": machines})))
        .map_err(api_error)
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Action {
    Connect {},
    AddMachine { name: String, host: String },
    ConnectMachine { machine_id: String, host: String },
    RenameMachine { machine_id: String, name: String },
    SetHost { machine_id: String, host: String },
    RemoveMachine { machine_id: String, confirm: bool },
    Disconnect { confirm: bool },
}

async fn action(State(state): State<WebState>, Json(action): Json<Action>) -> ApiResult {
    let manager = &state.manager;
    if !matches!(action, Action::Connect {} | Action::Disconnect { .. })
        && (manager.status_json().await["connected"] != true
            || !matches!(
                state.setup.borrow()["state"].as_str(),
                Some("ready" | "machine_selection")
            ))
    {
        return Err(api_error(
            "Wait for account setup to finish before connecting a machine",
        ));
    }
    match action {
        Action::Connect {} => {
            if manager.status_json().await["connected"] == true {
                return Err(api_error(
                    "Disconnect the current account before connecting another",
                ));
            }
            let _account = manager.account_operation().await;
            manager
                .primary()
                .begin_device_oauth()
                .await
                .map_err(api_error)?;
        }
        Action::AddMachine { name, host } => {
            manager.add_machine(&name, &host).await.map_err(api_error)?;
        }
        Action::ConnectMachine { machine_id, host } => {
            manager
                .connect_machine(&machine_id, &host)
                .await
                .map_err(api_error)?;
        }
        Action::RenameMachine { machine_id, name } => {
            manager
                .rename_machine(&machine_id, &name)
                .await
                .map_err(api_error)?;
        }
        Action::SetHost { machine_id, host } => {
            let host = normalize_host(&host).map_err(api_error)?;
            let _account = manager.account_operation().await;
            manager
                .engine(&machine_id)
                .await
                .map_err(api_error)?
                .set_host(&host)
                .await
                .map_err(api_error)?;
        }
        Action::RemoveMachine {
            machine_id,
            confirm: true,
        } => {
            manager
                .remove_machine(&machine_id)
                .await
                .map_err(api_error)?;
        }
        Action::Disconnect { confirm: true } => {
            return manager.disconnect_all().await.map(Json).map_err(api_error);
        }
        Action::RemoveMachine { .. } | Action::Disconnect { .. } => {
            return Err(api_error("Explicit confirmation is required"));
        }
    }
    Ok(Json(json!({"ok": true})))
}

async fn asset(State(state): State<WebState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if path != "index.html"
        && !(path.starts_with("assets/")
            && path.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            }))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let content_type = match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match tokio::fs::read(state.root.join(path)).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type)], Body::from(bytes)).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn router(state: WebState) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/machines", get(machines))
        .route("/api/action", post(action))
        .fallback(get(asset))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn_with_state(state.clone(), ingress_guard))
        .with_state(state)
}

pub(super) async fn serve(
    listener: TcpListener,
    manager: Arc<MachineManager>,
    setup: watch::Receiver<Value>,
    root: PathBuf,
) -> std::io::Result<()> {
    let state = WebState {
        manager,
        setup,
        root,
        csrf: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
    };
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
