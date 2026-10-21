//! `cove serve`: the HTTP + WebSocket API over the VM manager.

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
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
    pairing: Arc<std::sync::Mutex<cove_core::keys::Pairing>>,
    public: Option<Arc<Public>>,
}

/// Where clients on other computers reach this server over HTTPS, and the certificate
/// they should expect there.
pub struct Public {
    pub url: String,
    pub certs: Arc<cove_core::acme::Certs>,
}

impl Public {
    /// Set while the certificate is self-signed, which clients pin; `None` once a
    /// public authority signed it.
    pub fn fingerprint(&self) -> Option<String> {
        self.certs.fingerprint()
    }
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
        let status = if text.starts_with("no VM named")
            || text.starts_with("no process named")
            || text.starts_with("no snapshot named")
            || text.starts_with("no key")
        {
            StatusCode::NOT_FOUND
        } else if text.contains("is not running")
            || text.contains("already exists")
            || text.contains("is paused")
            || text.starts_with("stop ")
        {
            StatusCode::CONFLICT
        } else if text.starts_with("too many wrong pairing codes") {
            StatusCode::TOO_MANY_REQUESTS
        } else if text.starts_with("that pairing code") {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError(status, text)
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn router(manager: Arc<Manager>, key: String, public: Option<Arc<Public>>) -> Router {
    let app = App {
        manager,
        key: Arc::new(key),
        pairing: Default::default(),
        public,
    };
    let api = Router::new()
        .route("/images", get(images))
        .route("/images/{name}/pull", post(pull_image))
        .route("/vms", get(list).post(create))
        .route("/vms/{name}", get(status).delete(remove).patch(update))
        .route("/vms/{name}/start", post(start))
        .route("/vms/{name}/stop", post(stop))
        .route("/vms/{name}/pause", post(pause))
        .route("/vms/{name}/resume", post(resume))
        .route("/vms/{name}/rename", post(rename))
        .route("/vms/{name}/copy", post(copy))
        .route("/vms/{name}/snapshots", post(save_snapshot))
        .route("/snapshots", get(snapshots))
        .route("/snapshots/{name}", get(snapshot).delete(remove_snapshot))
        .route("/settings", get(settings).put(set_settings))
        .route("/keys", get(keys).post(create_key))
        .route("/keys/{id}", delete(remove_key))
        .route("/pair/codes", post(pair_code))
        .route("/logins", get(logins))
        .route("/logins/{agent}", put(import_login).delete(remove_login))
        .route("/vms/{name}/logins/sync", post(sync_logins))
        .route("/vms/{name}/exec", post(exec))
        .route("/vms/{name}/stream", get(stream))
        .route("/vms/{name}/procs", get(procs))
        .route("/vms/{name}/procs/{proc}", delete(kill))
        .route("/vms/{name}/files", get(read_file).put(write_file))
        .route("/vms/{name}/dir", get(list_dir))
        .route("/vms/{name}/ports", get(ports))
        .route("/vms/{name}/forwards", post(forward))
        .route("/vms/{name}/forwards/{port}", delete(unforward))
        .route(
            "/vms/{name}/egress",
            get(egress).put(set_egress).post(set_egress),
        )
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    Router::new()
        .route("/v1/health", get(health))
        // Needs no key: a pairing code is how a client gets one.
        .route("/v1/pair", post(pair))
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
    if !constant_time_eq(given.as_bytes(), app.key.as_bytes())
        && !cove_core::keys::valid(&app.manager.paths, given)
    {
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
    "egress",
    "pause",
    "auto-pause",
    "timeout",
    "logins",
    "labels",
    "rename",
    "snapshots",
    "settings",
    "keys",
    "pair",
    "https",
];

/// Also lists `backend:<name>` for each backend that can run VMs here, and the one
/// `auto` picks as `backend` (null when none can).
pub fn health_value() -> serde_json::Value {
    let mut features: Vec<String> = FEATURES.iter().map(|f| f.to_string()).collect();
    for b in cove_core::backend::platform_backends() {
        if cove_core::backend::check(b).is_ok() {
            features.push(format!("backend:{b}"));
        }
    }
    let auto = cove_core::backend::auto()
        .ok()
        .filter(|b| cove_core::backend::check(b).is_ok());
    serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": cove_proto::PROTOCOL_VERSION,
        "backend": auto,
        "features": features,
    })
}

async fn health() -> Json<serde_json::Value> {
    Json(health_value())
}

async fn images(State(app): State<App>) -> Json<Vec<image::ImageInfo>> {
    Json(image::list(&app.manager.paths))
}

#[derive(Deserialize)]
struct PullQuery {
    /// The backend to fetch the image for (`gvisor` wants a root filesystem, the
    /// others a disk). Default: the one `auto` picks.
    backend: Option<String>,
}

async fn pull_image(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(q): Query<PullQuery>,
) -> ApiResult<Vec<image::ImageInfo>> {
    // Fetch the form the backend VMs get here boots from.
    let backend = match q.backend.as_deref() {
        Some(b) => cove_core::backend::resolve(b)?,
        None => cove_core::backend::auto().unwrap_or(cove_core::backend::platform_default()),
    };
    image::pull_for(&app.manager.paths, &name, backend).await?;
    Ok(Json(image::list(&app.manager.paths)))
}

/// `?label=key=value`, repeated: only VMs with every one of those labels.
async fn list(
    State(app): State<App>,
    Query(query): Query<Vec<(String, String)>>,
) -> ApiResult<Vec<cove_core::VmStatus>> {
    let mut wanted = Vec::new();
    for (k, v) in &query {
        if k != "label" {
            continue;
        }
        match v.split_once('=') {
            Some((key, value)) => wanted.push((key.to_string(), value.to_string())),
            None => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "label filters are key=value".into(),
                ))
            }
        }
    }
    let vms = app.manager.list().await;
    Ok(Json(
        vms.into_iter()
            .filter(|vm| {
                wanted
                    .iter()
                    .all(|(k, v)| vm.labels.get(k).is_some_and(|have| have == v))
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateBody {
    #[serde(flatten)]
    spec: VmSpec,
    #[serde(default = "yes")]
    start: bool,
    /// Pause or delete it this many minutes from now.
    #[serde(default)]
    timeout_minutes: Option<u64>,
    #[serde(default)]
    on_timeout: cove_core::model::TimeoutAction,
}

fn yes() -> bool {
    true
}

/// Fields the request leaves out come from the server's settings.
async fn create(
    State(app): State<App>,
    Json(mut body): Json<serde_json::Value>,
) -> ApiResult<cove_core::VmStatus> {
    cove_core::settings::Settings::load(&app.manager.paths).apply(&mut body);
    let body: CreateBody = serde_json::from_value(body)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("bad request: {e}")))?;
    let mut spec = body.spec;
    if let Some(minutes) = body.timeout_minutes {
        let request = cove_core::model::TimeoutRequest {
            minutes,
            action: body.on_timeout,
        };
        spec.timeout = Some(
            request
                .resolve(cove_core::now(), cove_core::minute_secs())
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e))?,
        );
    }
    Ok(Json(app.manager.create(spec, body.start).await?))
}

async fn update(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(update): Json<cove_core::model::VmUpdate>,
) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.update(&name, update).await?))
}

async fn pause(State(app): State<App>, Path(name): Path<String>) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(
        app.manager
            .pause(&name, cove_core::model::PauseReason::User)
            .await?,
    ))
}

async fn resume(
    State(app): State<App>,
    Path(name): Path<String>,
) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.resume(&name).await?))
}

#[derive(Deserialize)]
struct RenameBody {
    to: String,
}

async fn rename(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult<cove_core::VmStatus> {
    Ok(Json(app.manager.rename(&name, &body.to).await?))
}

#[derive(Deserialize)]
struct SnapshotBody {
    name: String,
    #[serde(default)]
    description: String,
}

async fn save_snapshot(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(body): Json<SnapshotBody>,
) -> ApiResult<cove_core::snapshot::SnapshotInfo> {
    Ok(Json(
        app.manager
            .save_snapshot(&name, &body.name, &body.description)
            .await?,
    ))
}

async fn snapshots(State(app): State<App>) -> Json<Vec<cove_core::snapshot::SnapshotInfo>> {
    Json(cove_core::snapshot::list(&app.manager.paths))
}

async fn snapshot(
    State(app): State<App>,
    Path(name): Path<String>,
) -> ApiResult<cove_core::snapshot::SnapshotInfo> {
    Ok(Json(cove_core::snapshot::load(&app.manager.paths, &name)?))
}

async fn remove_snapshot(
    State(app): State<App>,
    Path(name): Path<String>,
) -> ApiResult<serde_json::Value> {
    cove_core::snapshot::remove(&app.manager.paths, &name)?;
    Ok(Json(serde_json::json!({ "deleted": name })))
}

async fn settings(State(app): State<App>) -> Json<cove_core::settings::Settings> {
    Json(cove_core::settings::Settings::load(&app.manager.paths))
}

/// Replaces every setting; send back what `GET` gave, changed.
async fn set_settings(
    State(app): State<App>,
    Json(settings): Json<cove_core::settings::Settings>,
) -> ApiResult<cove_core::settings::Settings> {
    settings.save(&app.manager.paths)?;
    Ok(Json(settings))
}

async fn logins(State(app): State<App>) -> Json<Vec<cove_core::logins::LoginInfo>> {
    Json(cove_core::logins::list(&app.manager.paths))
}

#[derive(Deserialize)]
struct LoginBody {
    /// File path from the home folder to its content, base64.
    files: std::collections::BTreeMap<String, cove_proto::Bytes>,
}

/// Save an agent's login files from elsewhere (another computer's login).
async fn import_login(
    State(app): State<App>,
    Path(agent): Path<String>,
    Json(body): Json<LoginBody>,
) -> ApiResult<Vec<cove_core::logins::LoginInfo>> {
    cove_core::logins::import(&app.manager.paths, &agent, body.files)?;
    sync_all_soon(&app);
    Ok(logins(State(app)).await)
}

/// Sign an agent out on this server: its files go, from every running VM too.
async fn remove_login(
    State(app): State<App>,
    Path(agent): Path<String>,
) -> ApiResult<serde_json::Value> {
    let removed = cove_core::logins::remove(&app.manager.paths, &agent)?;
    sync_all_soon(&app);
    Ok(Json(
        serde_json::json!({ "agent": agent, "removed": removed }),
    ))
}

/// Sync one VM's logins now, as after someone signed in there.
async fn sync_logins(
    State(app): State<App>,
    Path(name): Path<String>,
) -> ApiResult<Vec<cove_core::logins::LoginInfo>> {
    app.manager.sync_logins(&name).await?;
    Ok(logins(State(app)).await)
}

fn sync_all_soon(app: &App) {
    let manager = app.manager.clone();
    tokio::spawn(async move { manager.sync_all_logins().await });
}

async fn keys(State(app): State<App>) -> Json<Vec<cove_core::keys::KeyInfo>> {
    Json(cove_core::keys::list(&app.manager.paths))
}

#[derive(Deserialize)]
struct KeyBody {
    name: String,
}

/// The key itself is in the reply only; the server keeps its hash.
async fn create_key(
    State(app): State<App>,
    Json(body): Json<KeyBody>,
) -> ApiResult<serde_json::Value> {
    let (info, key) = cove_core::keys::create(&app.manager.paths, &body.name)?;
    Ok(Json(
        serde_json::json!({ "id": info.id, "name": info.name, "created_at": info.created_at, "key": key }),
    ))
}

async fn remove_key(
    State(app): State<App>,
    Path(id): Path<String>,
) -> ApiResult<cove_core::keys::KeyInfo> {
    Ok(Json(cove_core::keys::remove(&app.manager.paths, &id)?))
}

#[derive(Deserialize, Default)]
struct PairCodeBody {
    /// What the key it pairs is called, unless the client names it.
    #[serde(default)]
    name: Option<String>,
}

async fn pair_code(
    State(app): State<App>,
    body: Option<Json<PairCodeBody>>,
) -> Json<serde_json::Value> {
    let name = body
        .and_then(|b| b.0.name)
        .unwrap_or_else(|| "paired client".into());
    let (code, expires_at) = app.pairing.lock().unwrap().issue(&name);
    let mut v = serde_json::json!({ "code": code, "expires_at": expires_at });
    if let Some(p) = &app.public {
        let fp = p.fingerprint();
        v["url"] = p.url.clone().into();
        v["pair"] = pair_string(&p.url, &code, fp.as_deref()).into();
        v["fingerprint"] = fp.into();
    }
    Json(v)
}

/// One string with everything a client needs to pair: where the server is, the code,
/// and the certificate to expect. `cove login` and Runo take it as it is.
/// `cove://203.0.113.7:7443/K7QF-2MXD?fp=sha256:9f2c…`
pub fn pair_string(url: &str, code: &str, fingerprint: Option<&str>) -> String {
    let host = url.trim_start_matches("https://").trim_end_matches('/');
    match fingerprint {
        Some(fp) => format!("cove://{host}/{code}?fp={fp}"),
        None => format!("cove://{host}/{code}"),
    }
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
    /// What to call the new key, such as "Runo on Anish's MacBook".
    #[serde(default)]
    name: Option<String>,
}

/// Trade a one-time code for a named API key of the client's own.
async fn pair(State(app): State<App>, Json(body): Json<PairBody>) -> ApiResult<serde_json::Value> {
    let issued_name = app.pairing.lock().unwrap().redeem(&body.code)?;
    let name = body
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(issued_name);
    let (info, key) = cove_core::keys::create(&app.manager.paths, &name)?;
    Ok(Json(serde_json::json!({
        "id": info.id,
        "name": info.name,
        "key": key,
        "server": health_value(),
    })))
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
    let guest = app.manager.guest(&name).await?;
    // A long command keeps the VM busy for idle stop, like an open stream.
    let _open = app.manager.stream_opened(&name);
    Ok(Json(guest.exec(command).await?))
}

async fn procs(State(app): State<App>, Path(name): Path<String>) -> ApiResult<serde_json::Value> {
    let guest = app.manager.guest(&name).await?;
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
    let guest = app.manager.guest(&name).await?;
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
    let guest = app.manager.guest(&name).await?;
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
    let guest = app.manager.guest(&name).await?;
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
    let guest = app.manager.guest(&name).await?;
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

async fn egress(
    State(app): State<App>,
    Path(name): Path<String>,
) -> ApiResult<cove_core::manager::EgressReport> {
    Ok(Json(app.manager.egress(&name).await?))
}

#[derive(Deserialize)]
struct EgressBody {
    allow: Vec<String>,
}

async fn set_egress(
    State(app): State<App>,
    Path(name): Path<String>,
    Json(body): Json<EgressBody>,
) -> ApiResult<cove_core::manager::EgressReport> {
    Ok(Json(app.manager.set_allow(&name, body.allow).await?))
}

/// WebSocket bridge to the guest. The first text message is a `Request` (exec, spawn
/// or attach); after that, guest frames and client `Input` lines pass straight through.
async fn stream(
    State(app): State<App>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let guest = app.manager.guest(&name).await?;
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

/// `cove serve --https`: where to listen, and the name clients use to reach it.
pub struct HttpsOptions {
    pub listen: String,
    pub public_host: Option<String>,
    /// Get a certificate for this domain, or for the public IP with `ip`.
    pub domain: Option<String>,
    pub acme_email: Option<String>,
}

pub async fn serve(manager: Arc<Manager>, listen: &str, https: Option<HttpsOptions>) -> Result<()> {
    let key = manager.paths.load_or_create_api_key()?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    let mut tls = None;
    let mut public = None;
    if let Some(opts) = https {
        let tcp = tokio::net::TcpListener::bind(&opts.listen).await?;
        let bound = tcp.local_addr()?;
        let ip_cert = opts.domain.as_deref() == Some("ip");
        let host = opts
            .public_host
            .clone()
            .or_else(|| opts.domain.clone().filter(|_| !ip_cert))
            .unwrap_or_else(|| public_ip(bound.ip()).to_string());
        let tls_dir = manager.paths.root.join("tls");
        let cert =
            cove_core::https::load_or_create_self_signed(&tls_dir, std::slice::from_ref(&host))?;
        let certs = cove_core::acme::Certs::new(&cert)?;
        if let Some(domain) = &opts.domain {
            let name = cove_core::acme::Name::parse(if ip_cert { &host } else { domain })?;
            let directory = std::env::var("COVE_ACME_DIRECTORY")
                .unwrap_or_else(|_| cove_core::acme::LETS_ENCRYPT.to_string());
            if bound.port() != 443 && directory == cove_core::acme::LETS_ENCRYPT {
                eprintln!(
                    "warning: Let's Encrypt checks {name} on port 443, and HTTPS listens on \
                     {bound}. Forward port 443 here, or listen on 0.0.0.0:443"
                );
            }
            let acme = cove_core::acme::Acme {
                name,
                directory,
                email: opts.acme_email.clone(),
                root: std::env::var_os("COVE_ACME_ROOT").map(Into::into),
                dir: tls_dir.join("acme"),
            };
            acme.load_saved(&certs);
            tokio::spawn(acme.run(certs.clone(), |line| eprintln!("{line}")));
        }
        let p = Public {
            url: https_url(&host, bound.port()),
            certs: certs.clone(),
        };
        eprintln!(
            "cove serving HTTPS on {bound}, for clients at {}{}",
            p.url,
            match p.fingerprint() {
                Some(fp) => format!(" (self-signed certificate {fp})"),
                None => String::new(),
            }
        );
        tls = Some(TlsListener::spawn(tcp, certs.server_config()?));
        public = Some(Arc::new(p));
    }
    std::fs::write(
        manager.paths.server_file(),
        serde_json::to_vec_pretty(&serde_json::json!({
            "url": url,
            "pid": std::process::id(),
            "https_url": public.as_ref().map(|p| p.url.clone()),
            "fingerprint": public.as_ref().and_then(|p| p.fingerprint()),
        }))?,
    )?;
    eprintln!(
        "cove serving on {url} (data in {})",
        manager.paths.root.display()
    );
    tokio::spawn(check_timers(manager.clone()));
    // VMs that kept running while the server was down need their proxies back.
    manager.restart_proxies().await;
    let app = router(manager, key, public);
    match tls {
        Some(tls) => {
            let plain =
                axum::serve(listener, app.clone()).with_graceful_shutdown(shutdown_signal());
            let secure = axum::serve(tls, app).with_graceful_shutdown(shutdown_signal());
            tokio::try_join!(async { plain.await }, async { secure.await })?;
        }
        None => {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?
        }
    }
    Ok(())
}

fn https_url(host: &str, port: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match port {
        443 => format!("https://{host}"),
        p => format!("https://{host}:{p}"),
    }
}

/// The address other computers most likely reach us at: the bound one, or for
/// `0.0.0.0` the one this machine sends from to the internet (no packet is sent).
fn public_ip(bound: std::net::IpAddr) -> std::net::IpAddr {
    if !bound.is_unspecified() {
        return bound;
    }
    let probe = |target: &str, local: &str| {
        let s = std::net::UdpSocket::bind(local).ok()?;
        s.connect(target).ok()?;
        s.local_addr().ok().map(|a| a.ip())
    };
    let found = if bound.is_ipv4() {
        probe("1.1.1.1:80", "0.0.0.0:0")
    } else {
        probe("[2606:4700:4700::1111]:80", "[::]:0").or_else(|| probe("1.1.1.1:80", "0.0.0.0:0"))
    };
    found.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

/// A listener that does the TLS handshake off the accept loop, so one slow client
/// can't hold up the rest.
struct TlsListener {
    rx: tokio::sync::mpsc::Receiver<(
        tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        std::net::SocketAddr,
    )>,
    addr: std::net::SocketAddr,
}

impl TlsListener {
    fn spawn(tcp: tokio::net::TcpListener, config: Arc<rustls::ServerConfig>) -> TlsListener {
        let addr = tcp.local_addr().expect("bound listener");
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        tokio::spawn(async move {
            loop {
                let Ok((conn, peer)) = tcp.accept().await else {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                };
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let handshake = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        acceptor.accept(conn),
                    );
                    if let Ok(Ok(tls)) = handshake.await {
                        let _ = tx.send((tls, peer)).await;
                    }
                });
            }
        });
        TlsListener { rx, addr }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(conn) => conn,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.addr)
    }
}

/// Check idle timers and timeouts every `COVE_IDLE_CHECK_SECS` (30 by default).
/// `COVE_IDLE_MINUTE_SECS` shortens a minute so tests don't wait for real ones.
async fn check_timers(manager: Arc<Manager>) {
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
        for (name, what) in manager.check_timers(minute).await {
            eprintln!("{name}: {what}");
        }
        // Tokens agents refreshed reach the other VMs.
        manager.sync_all_logins().await;
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
