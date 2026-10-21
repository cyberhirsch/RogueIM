//! rim-drop: the user's own drop for "send to anyone" (PRD HB-3), e.g. on a
//! Raspberry Pi behind a Cloudflare tunnel. It stores what RogueIM uploads,
//! which is ciphertext only (the key stays in the link's `#fragment`), and
//! hands it out to anyone with the link until it expires or its downloads are
//! used up. Uploads need the token from RIM_DROP_TOKEN; downloads need nothing.
//!
//!   POST   /api/files            {size, expires_secs, max_downloads} -> 201 {id}
//!   PUT    /api/files/<id>?offset=N   the next part (at most 64 MiB)
//!   POST   /api/files/<id>/done
//!   DELETE /api/files/<id>
//!   GET    /api/check            token check
//!   GET    /f/<id>               the ciphertext, for the download page
//!   GET    /health

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::TryStreamExt;
use http_body_util::{combinators::BoxBody, BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rim_drop::B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

type Body = BoxBody<Bytes, std::io::Error>;

const MAX_PART: u64 = 64 << 20;
const MAX_EXPIRY: u64 = 90 * 86_400;
/// Unfinished uploads are dropped after a day.
const STALE: u64 = 86_400;

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    size: u64,
    written: u64,
    created: u64,
    expires: u64,
    max_downloads: u32,
    downloads: u32,
    done: bool,
}

struct State {
    dir: PathBuf,
    token: String,
    quota: u64,
    max_file: u64,
    lock: Mutex<()>,
}

#[derive(Deserialize)]
struct NewFile {
    size: u64,
    expires_secs: u64,
    #[serde(default)]
    max_downloads: u32,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn text(status: StatusCode, body: impl Into<String>) -> Response<Body> {
    let body: String = body.into();
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Access-Control-Allow-Origin", "*")
        .body(Full::new(Bytes::from(body)).map_err(|n| match n {}).boxed())
        .unwrap()
}

fn json(status: StatusCode, v: serde_json::Value) -> Response<Body> {
    let mut r = text(status, v.to_string());
    r.headers_mut().insert("Content-Type", "application/json".parse().unwrap());
    r
}

fn valid_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl State {
    fn meta_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
    fn data_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.bin"))
    }
    fn load(&self, id: &str) -> Option<Entry> {
        std::fs::read(self.meta_path(id)).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }
    fn save(&self, id: &str, e: &Entry) {
        let tmp = self.dir.join(format!("{id}.json.tmp"));
        if std::fs::write(&tmp, serde_json::to_vec(e).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, self.meta_path(id));
        }
    }
    fn remove(&self, id: &str) {
        let _ = std::fs::remove_file(self.data_path(id));
        let _ = std::fs::remove_file(self.meta_path(id));
    }
    fn entries(&self) -> Vec<(String, Entry)> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return vec![] };
        rd.flatten()
            .filter_map(|f| {
                let name = f.file_name().to_string_lossy().to_string();
                let id = name.strip_suffix(".json")?.to_string();
                Some((id.clone(), self.load(&id)?))
            })
            .collect()
    }
    fn authorized(&self, req: &Request<Incoming>) -> bool {
        req.headers()
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| same(t, &self.token))
    }
    /// Deletes what expired, what reached its download limit and unfinished uploads.
    fn sweep(&self) {
        let t = now();
        for (id, e) in self.entries() {
            let used_up = e.max_downloads > 0 && e.downloads >= e.max_downloads;
            if t >= e.expires || used_up || (!e.done && t > e.created + STALE) {
                self.remove(&id);
            }
        }
    }
}

async fn handle(st: Arc<State>, req: Request<Incoming>) -> Result<Response<Body>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let r = match (req.method().clone(), parts.as_slice()) {
        (Method::GET, ["health"]) => text(StatusCode::OK, "ok"),
        (Method::OPTIONS, ["f", _]) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Methods", "GET")
            .body(Full::new(Bytes::new()).map_err(|n| match n {}).boxed())
            .unwrap(),
        (Method::GET, ["f", id]) => download(&st, id).await,
        (_, ["api", ..]) if !st.authorized(&req) => text(StatusCode::UNAUTHORIZED, "token needed"),
        (Method::GET, ["api", "check"]) => text(StatusCode::OK, "ok"),
        (Method::POST, ["api", "files"]) => create(&st, req).await,
        (Method::PUT, ["api", "files", id]) => {
            let id = id.to_string();
            put(&st, &id, req).await
        }
        (Method::POST, ["api", "files", id, "done"]) => done(&st, id).await,
        (Method::DELETE, ["api", "files", id]) if valid_id(id) => {
            let _g = st.lock.lock().await;
            st.remove(id);
            text(StatusCode::OK, "deleted")
        }
        _ => text(StatusCode::NOT_FOUND, "not found"),
    };
    Ok(r)
}

async fn create(st: &State, req: Request<Incoming>) -> Response<Body> {
    let Ok(body) = http_body_util::Limited::new(req.into_body(), 4096).collect().await else {
        return text(StatusCode::BAD_REQUEST, "request too large");
    };
    let Ok(n) = serde_json::from_slice::<NewFile>(&body.to_bytes()) else {
        return text(StatusCode::BAD_REQUEST, "expected {size, expires_secs, max_downloads}");
    };
    if n.size == 0 || n.size > st.max_file {
        return text(StatusCode::PAYLOAD_TOO_LARGE, format!("files up to {} GB", st.max_file >> 30));
    }
    let _g = st.lock.lock().await;
    st.sweep();
    let used: u64 = st.entries().iter().map(|(_, e)| e.size).sum();
    if used + n.size > st.quota {
        return text(StatusCode::INSUFFICIENT_STORAGE, format!("the drop is full ({} of {} GB used)", used >> 30, st.quota >> 30));
    }
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).expect("random");
    let id = B64.encode(raw);
    let t = now();
    let e = Entry { size: n.size, written: 0, created: t, expires: t + n.expires_secs.clamp(60, MAX_EXPIRY), max_downloads: n.max_downloads, downloads: 0, done: false };
    if std::fs::File::create(st.data_path(&id)).is_err() {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "cannot write");
    }
    st.save(&id, &e);
    json(StatusCode::CREATED, serde_json::json!({ "id": id, "expires": e.expires }))
}

async fn put(st: &State, id: &str, req: Request<Incoming>) -> Response<Body> {
    if !valid_id(id) {
        return text(StatusCode::NOT_FOUND, "not found");
    }
    let offset: u64 = req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("offset="))).and_then(|v| v.parse().ok()).unwrap_or(u64::MAX);
    let Some(mut e) = st.load(id) else { return text(StatusCode::NOT_FOUND, "not found") };
    if e.done || offset != e.written {
        return text(StatusCode::CONFLICT, format!("expected offset {}", e.written));
    }
    let Ok(mut f) = tokio::fs::OpenOptions::new().append(true).open(st.data_path(id)).await else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "cannot write");
    };
    let mut body = req.into_body();
    let mut n = 0u64;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return text(StatusCode::BAD_REQUEST, "upload broke off") };
        if let Some(data) = frame.data_ref() {
            n += data.len() as u64;
            if n > MAX_PART || e.written + n > e.size {
                let _ = f.set_len(e.written).await;
                return text(StatusCode::PAYLOAD_TOO_LARGE, "part too large");
            }
            if f.write_all(data).await.is_err() {
                return text(StatusCode::INSUFFICIENT_STORAGE, "cannot write");
            }
        }
    }
    let _ = f.flush().await;
    let _g = st.lock.lock().await;
    e.written += n;
    st.save(id, &e);
    json(StatusCode::OK, serde_json::json!({ "written": e.written }))
}

async fn done(st: &State, id: &str) -> Response<Body> {
    let _g = st.lock.lock().await;
    let Some(mut e) = st.load(id).filter(|_| valid_id(id)) else { return text(StatusCode::NOT_FOUND, "not found") };
    if e.written != e.size {
        return text(StatusCode::CONFLICT, format!("{} of {} bytes arrived", e.written, e.size));
    }
    e.done = true;
    st.save(id, &e);
    text(StatusCode::OK, "ok")
}

async fn download(st: &State, id: &str) -> Response<Body> {
    if !valid_id(id) {
        return text(StatusCode::NOT_FOUND, "no such file");
    }
    let e = {
        let _g = st.lock.lock().await;
        let Some(mut e) = st.load(id).filter(|e| e.done) else { return text(StatusCode::NOT_FOUND, "no such file") };
        if now() >= e.expires || (e.max_downloads > 0 && e.downloads >= e.max_downloads) {
            return text(StatusCode::GONE, "this link has expired");
        }
        e.downloads += 1;
        st.save(id, &e);
        e
    };
    let Ok(f) = tokio::fs::File::open(st.data_path(id)).await else { return text(StatusCode::NOT_FOUND, "no such file") };
    let stream = tokio_util::io::ReaderStream::with_capacity(f, 256 << 10).map_ok(Frame::data);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", e.size)
        .header("Cache-Control", "no-store")
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Expose-Headers", "Content-Length")
        .body(StreamBody::new(stream).boxed())
        .unwrap()
}

fn arg(name: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == name {
            return it.next();
        }
    }
    None
}

pub async fn serve(dir: &Path, token: String, listen: SocketAddr, quota: u64, max_file: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let st = Arc::new(State { dir: dir.to_path_buf(), token, quota, max_file, lock: Mutex::new(()) });
    let sweeper = st.clone();
    tokio::spawn(async move {
        loop {
            {
                let _g = sweeper.lock.lock().await;
                sweeper.sweep();
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!("rim-drop: {} on {}", dir.display(), listener.local_addr()?);
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let st = st.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| handle(st.clone(), req));
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
        });
    }
}

#[tokio::main]
async fn main() {
    let token = std::env::var("RIM_DROP_TOKEN").unwrap_or_default();
    if token.len() < 24 {
        eprintln!("rim-drop: set RIM_DROP_TOKEN to a random secret of at least 24 characters (e.g. openssl rand -base64 32).");
        std::process::exit(2);
    }
    let dir = PathBuf::from(arg("--dir").unwrap_or_else(|| "/data".into()));
    let listen: SocketAddr = arg("--listen").unwrap_or_else(|| "0.0.0.0:8080".into()).parse().expect("--listen host:port");
    let gb = |name: &str, default: u64| arg(name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(default) << 30;
    if let Err(e) = serve(&dir, token, listen, gb("--quota-gb", 100), gb("--max-file-gb", 20)).await {
        eprintln!("rim-drop: {e}");
        std::process::exit(1);
    }
}
