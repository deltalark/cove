//! `cove serve`: the HTTP + WebSocket API over the VM manager.

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use cove_core::model::VmSpec;
use cove_core::{image, Manager};
use cove_proto::{Bytes, Frame};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Clone)]
struct App {
    manager: Arc<Manager>,
    key: Arc<String>,
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let text = format!("{e:#}");
        let status = if text.starts_with("no VM named") || text.starts_with("no process named") {
            StatusCode::NOT_FOUND
        } else if text.contains("is not running") || text.contains("already exists") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(status, text)
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(manager: Arc<Manager>, key: String) -> Router {
    let app = App {
        manager,
        key: Arc::new(key),
    };
    let api = Router::new()
        .route("/images", get(images))
        .route("/images/{name}/pull", post(pull_image))
        .route("/vms", get(list).post(create))
        .route("/vms/{name}", get(status).delete(remove))
        .route("/vms/{name}/start", post(start))
        .route("/vms/{name}/stop", post(stop))
        .route("/vms/{name}/copy", post(copy))
        .route("/vms/{name}/exec", post(exec))
        .route("/vms/{name}/stream", get(stream))
        .route("/vms/{name}/procs", get(procs))
        .route("/vms/{name}/procs/{proc}", delete(kill))
        .route("/vms/{name}/files", get(read_file).put(write_file))
        .route("/vms/{name}/dir", get(list_dir))
        .route("/vms/{name}/ports", get(ports))
        .route("/vms/{name}/forwards", post(forward))
        .route("/vms/{name}/forwards/{port}", delete(unforward))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/v1/health", get(health))
        .nest("/v1", api)
        .with_state(app)
}

async fn auth(
    State(app): State<App>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let given = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(given.as_bytes(), app.key.as_bytes()) {
        return ApiError(StatusCode::UNAUTHORIZED, "missing or wrong API key".into())
            .into_response();
    }
    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What this server can do, so clients (Runo, scripts) can check before relying on
/// something newer than the oldest server they talk to. Only ever add to this.
pub const FEATURES: &[&str] = &[
    "network",
    "idle-stop",
    "spawn",
    "attach",
    "forward",
    "copy",
    "files",
    "stdin-eof",
];

pub fn health_value() -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": cove_proto::PROTOCOL_VERSION,
        "backend": cove_core::backend::platform_default(),
        "features": FEATURES,
    })
}

async fn health() -> Json<serde_json::Value> {
    Json(health_value())
}

async fn images(State(app): State<App>) -> Json<Vec<image::ImageInfo>> {
    Json(image::list(&app.manager.paths))
}

async fn pull_image(
    State(app): State<App>,
    Path(name): Path<String>,
) -> ApiResult<Vec<image::ImageInfo>> {
    image::pull(&app.manager.paths, &name, &|_, _| {}).await?;
    Ok(Json(image::list(&app.manager.paths)))
}

async fn list(State(app): State<App>) -> Json<Vec<cove_core::VmStatus>> {
    Json(app.manager.list().await)
}

#[derive(Deserialize)]
struct CreateBody {
    #[serde(flatten)]
    spec: VmSpec,
    #[serde(default = "yes")]
    start: bool,
}

fn yes() -> bool {
    true
}

async fn create(
    State(app): State<App>,
    Json(body): Json<CreateBody>,
) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.create(body.spec, body.start).await?))
}

async fn status(
    State(app): State<App>,
    Path(name): Path<String>,
) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.status(&name).await?))
}

async fn remove(State(app): State<App>, Path(name): Path<String>) -> ApiResult<serde_json::Value> {
    app.manager.delete(&name).await?;
    Ok(Json(serde_json::json!({ "deleted": name })))
}

async fn start(State(app): State<App>, Path(name): Path<String>) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.start(&name).await?))
}

async fn stop(State(app): State<App>, Path(name): Path<String>) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.stop(&name).await?))
}

#[derive(Deserialize)]
struct CopyBody {
    to: String,
}

async fn copy(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(body): Json<CopyBody>,
) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.copy(&name, &body.to).await?))
}

async fn exec(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(command): Json<cove_proto::Command>,
) -> ApiResult<cove_core::guest::ExecOutput> {
    let guest = app.manager.guest(&name)?;
    // A long command keeps the VM busy for idle stop, like an open stream.
    let _open = app.manager.stream_opened(&name);
    Ok(Json(guest.exec(command).await?))
}

async fn procs(State(app): State<App>, Path(name): Path<String>) -> ApiResult<serde_json::Value> {
    let guest = app.manager.guest(&name)?;
    Ok(Json(guest.call(&cove_proto::Request::Procs).await?))
}

#[derive(Deserialize)]
struct KillQuery {
    signal: Option<i32>,
}

async fn kill(
    State(app): State<App>,
    Path((name, proc)): Path<(String, String)>,
    Query(q): Query<KillQuery>,
) -> ApiResult<serde_json::Value> {
    let guest = app.manager.guest(&name)?;
    Ok(Json(
        guest
            .call(&cove_proto::Request::Kill {
                name: proc,
                signal: q.signal,
            })
            .await?,
    ))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
    mode: Option<String>,
}

async fn read_file(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<Vec<u8>, ApiError> {
    let guest = app.manager.guest(&name)?;
    let data: Bytes = guest
        .call_as(&cove_proto::Request::ReadFile { path: q.path })
        .await?;
    Ok(data.0)
}

async fn write_file(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(q): Query<FileQuery>,
    body: axum::body::Bytes,
) -> ApiResult<serde_json::Value> {
    let guest = app.manager.guest(&name)?;
    let mode = match q.mode.as_deref() {
        Some(m) => Some(
            u32::from_str_radix(m, 8)
                .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "mode is octal, e.g. 755".into()))?,
        ),
        None => None,
    };
    let request = cove_proto::Request::WriteFile {
        path: q.path,
        data: Bytes(body.to_vec()),
        mode,
    };
    Ok(Json(guest.call(&request).await?))
}

async fn list_dir(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(q): Query<FileQuery>,
) -> ApiResult<serde_json::Value> {
    let guest = app.manager.guest(&name)?;
    Ok(Json(
        guest
            .call(&cove_proto::Request::ListDir { path: q.path })
            .await?,
    ))
}

async fn ports(State(app): State<App>, Path(name): Path<String>) -> ApiResult<serde_json::Value> {
    let ports = app.manager.ports(&name).await?;
    let forwards = app.manager.status(&name).await?.forwards;
    Ok(Json(
        serde_json::json!({ "listening": ports, "forwards": forwards }),
    ))
}

#[derive(Deserialize)]
struct ForwardBody {
    guest_port: u16,
}

async fn forward(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(body): Json<ForwardBody>,
) -> ApiResult<cove_core::model::Forward> {
    Ok(Json(app.manager.forward(&name, body.guest_port).await?))
}

async fn unforward(
    State(app): State<App>,
    Path((name, port)): Path<(String, u16)>,
) -> ApiResult<serde_json::Value> {
    app.manager.unforward(&name, port).await;
    Ok(Json(serde_json::json!({ "removed": port })))
}

/// WebSocket bridge to the guest. The first text message is a `Request` (exec, spawn
/// or attach); after that, guest frames and client `Input` lines pass straight through.
async fn stream(
    State(app): State<App>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let guest = app.manager.guest(&name)?;
    let open = app.manager.stream_opened(&name);
    Ok(ws.on_upgrade(move |socket| async move {
        bridge(socket, guest).await;
        drop(open);
    }))
}

async fn bridge(socket: WebSocket, guest: cove_core::guest::GuestAddr) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let first = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(t))) => break t.to_string(),
            Some(Ok(_)) => continue,
            _ => return,
        }
    };
    let request: cove_proto::Request = match serde_json::from_str(&first) {
        Ok(
            r @ (cove_proto::Request::Exec(_)
            | cove_proto::Request::Spawn { .. }
            | cove_proto::Request::Attach { .. }),
        ) => r,
        Ok(_) => {
            let _ = send_frame(
                &mut ws_tx,
                &Frame::error("streams carry exec, spawn or attach"),
            )
            .await;
            return;
        }
        Err(e) => {
            let _ = send_frame(&mut ws_tx, &Frame::error(format!("bad request: {e}"))).await;
            return;
        }
    };
    let stream = match guest.open(&request).await {
        Ok(s) => s,
        Err(e) => {
            let _ = send_frame(&mut ws_tx, &Frame::error(format!("{e:#}"))).await;
            return;
        }
    };
    let (guest_rx, mut guest_tx) = tokio::io::split(stream);
    let upstream = tokio::spawn(async move {
        while let Some(Ok(message)) = ws_rx.next().await {
            let text = match message {
                Message::Text(t) => t.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            if guest_tx
                .write_all(text.trim_end().as_bytes())
                .await
                .is_err()
                || guest_tx.write_all(b"\n").await.is_err()
                || guest_tx.flush().await.is_err()
            {
                break;
            }
        }
        let _ = guest_tx.shutdown().await;
    });
    let mut lines = BufReader::new(guest_rx).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        let last = matches!(
            serde_json::from_str::<Frame>(&text),
            Ok(Frame::Exit { .. } | Frame::Error { .. })
        );
        if ws_tx.send(Message::Text(text.into())).await.is_err() {
            break;
        }
        if last {
            break;
        }
    }
    let _ = ws_tx.send(Message::Close(None)).await;
    // Let the client answer the close before the socket goes away; dropping it with
    // unread input would reset the connection and lose output still in flight.
    let abort = upstream.abort_handle();
    if tokio::time::timeout(std::time::Duration::from_secs(2), upstream)
        .await
        .is_err()
    {
        abort.abort();
    }
}

async fn send_frame(tx: &mut (impl SinkExt<Message> + Unpin), frame: &Frame) -> Result<(), ()> {
    let text = serde_json::to_string(frame).map_err(|_| ())?;
    tx.send(Message::Text(text.into())).await.map_err(|_| ())
}

pub async fn serve(manager: Arc<Manager>, listen: &str) -> Result<()> {
    let key = manager.paths.load_or_create_api_key()?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    std::fs::write(
        manager.paths.server_file(),
        serde_json::to_vec_pretty(&serde_json::json!({ "url": url, "pid": std::process::id() }))?,
    )?;
    eprintln!(
        "cove serving on {url} (data in {})",
        manager.paths.root.display()
    );
    tokio::spawn(stop_idle_vms(manager.clone()));
    axum::serve(listener, router(manager, key))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Check for idle VMs every `COVE_IDLE_CHECK_SECS` (30 by default).
/// `COVE_IDLE_MINUTE_SECS` shortens a minute so tests don't wait for real ones.
async fn stop_idle_vms(manager: Arc<Manager>) {
    let secs = |var: &str, default: u64| {
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default)
    };
    let every = std::time::Duration::from_secs(secs("COVE_IDLE_CHECK_SECS", 30));
    let minute = std::time::Duration::from_secs(secs("COVE_IDLE_MINUTE_SECS", 60));
    loop {
        tokio::time::sleep(every).await;
        for name in manager.stop_idle(minute).await {
            eprintln!("stopped {name}: idle");
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn health_lists_features() {
        let v = super::health_value();
        assert_eq!(v["ok"], true);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        let features: Vec<&str> = v["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        for f in ["network", "idle-stop", "spawn", "attach", "forward", "copy"] {
            assert!(features.contains(&f), "{f} missing");
        }
    }
}
