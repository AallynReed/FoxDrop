use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use futures_util::TryStreamExt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

const URL_SAFE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_');
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, sync::RwLock};
use tower::ServiceExt;
use tower_http::services::ServeFile;

const RESERVED: &[&str] = &["admin", "api", "info", "healthz", "favicon.ico", "favicon.png", "icon.png", "robots.txt", "static"];
const X_ROBOTS: header::HeaderName = header::HeaderName::from_static("x-robots-tag");
const INDEX_HTML: &str = include_str!("index.html");
const FAVICON: &[u8] = include_bytes!("favicon.png");
const ICON: &[u8] = include_bytes!("icon-256.png");

fn png(bytes: &'static [u8]) -> Response {
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=86400")], bytes).into_response()
}

#[derive(Clone, Serialize, Deserialize)]
struct Link {
    slug: String,
    path: String,
    created: u64,
    #[serde(default)]
    expires: Option<u64>,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    last_download: Option<u64>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Hashes {
    size: u64,
    mtime: u64,
    md5: String,
    sha1: String,
    sha256: String,
    sha512: String,
}

struct App {
    files: PathBuf,
    db: PathBuf,
    hash_db: PathBuf,
    public_url: String,
    /// Same links on a second host, shown as a backup.
    backup_url: Option<String>,
    links: RwLock<BTreeMap<String, Link>>,
    hashes: RwLock<BTreeMap<String, Hashes>>,
    /// path → bytes hashed so far, for files being hashed now or queued.
    hashing: std::sync::Mutex<std::collections::HashMap<String, Arc<AtomicU64>>>,
    hash_slot: tokio::sync::Semaphore,
}

enum HashState {
    Done(Hashes),
    Running(u64),
}

fn stat(full: &Path) -> Option<(u64, u64)> {
    let m = full.metadata().ok()?;
    Some((m.len(), m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs()))
}

fn hash_file(full: &Path, progress: &AtomicU64) -> std::io::Result<(String, String, String, String)> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(full)?;
    let (mut md5, mut sha1, mut sha256, mut sha512) =
        (md5::Md5::new(), sha1::Sha1::new(), sha2::Sha256::new(), sha2::Sha512::new());
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let b = &buf[..n];
        md5.update(b);
        sha1.update(b);
        sha256.update(b);
        sha512.update(b);
        progress.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok((hex::encode(md5.finalize()), hex::encode(sha1.finalize()), hex::encode(sha256.finalize()), hex::encode(sha512.finalize())))
}

/// Cached hashes if still valid for the file on disk, otherwise starts (or reports) the background hash.
async fn hash_state(app: &Shared, rel: &str, full: &Path) -> Option<HashState> {
    let (size, mtime) = stat(full)?;
    if let Some(h) = app.hashes.read().await.get(rel) {
        if h.size == size && h.mtime == mtime {
            return Some(HashState::Done(h.clone()));
        }
    }
    let progress = {
        let mut running = app.hashing.lock().unwrap();
        if let Some(p) = running.get(rel) {
            return Some(HashState::Running(p.load(Ordering::Relaxed)));
        }
        let p = Arc::new(AtomicU64::new(0));
        running.insert(rel.to_string(), p.clone());
        p
    };
    let (app2, rel2, full2) = (app.clone(), rel.to_string(), full.to_path_buf());
    tokio::spawn(async move {
        let _slot = app2.hash_slot.acquire().await;
        let p = progress.clone();
        let f = full2.clone();
        let res = tokio::task::spawn_blocking(move || hash_file(&f, &p)).await;
        if let (Ok(Ok((md5, sha1, sha256, sha512))), Some((size, mtime))) = (res, stat(&full2)) {
            let mut all = app2.hashes.write().await;
            all.insert(rel2.clone(), Hashes { size, mtime, md5, sha1, sha256, sha512 });
            all.retain(|k, _| app2.resolve(k).is_some());
            if let Ok(data) = serde_json::to_vec_pretty(&*all) {
                let tmp = app2.hash_db.with_extension("json.tmp");
                if tokio::fs::write(&tmp, data).await.is_ok() {
                    let _ = tokio::fs::rename(&tmp, &app2.hash_db).await;
                }
            }
        }
        app2.hashing.lock().unwrap().remove(&rel2);
    });
    Some(HashState::Running(0))
}

type Shared = Arc<App>;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

impl App {
    async fn save(&self, links: &BTreeMap<String, Link>) -> std::io::Result<()> {
        let tmp = self.db.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(&links.values().collect::<Vec<_>>())?;
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &self.db).await
    }

    /// Relative path → absolute file inside the root, refusing anything that escapes it.
    fn resolve(&self, rel: &str) -> Option<PathBuf> {
        let rel = Path::new(rel);
        if rel.as_os_str().is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            return None;
        }
        let root = self.files.canonicalize().ok()?;
        let full = root.join(rel).canonicalize().ok()?;
        (full.starts_with(&root) && full.is_file()).then_some(full)
    }

    fn resolve_dir(&self, rel: &str) -> Option<PathBuf> {
        let root = self.files.canonicalize().ok()?;
        if rel.is_empty() {
            return Some(root);
        }
        let p = Path::new(rel);
        if !p.components().all(|c| matches!(c, Component::Normal(_))) {
            return None;
        }
        let full = root.join(p).canonicalize().ok()?;
        (full.starts_with(&root) && full.is_dir()).then_some(full)
    }

    fn rel_of(&self, full: &Path) -> Option<String> {
        let root = self.files.canonicalize().ok()?;
        let rel = full.strip_prefix(&root).ok()?;
        Some(rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
    }

    fn url(&self, slug: &str) -> String {
        format!("{}/{}", self.public_url.trim_end_matches('/'), utf8_percent_encode(slug, URL_SAFE))
    }

    fn backup(&self, slug: &str) -> Option<String> {
        self.backup_url.as_ref().map(|b| format!("{}/{}", b.trim_end_matches('/'), utf8_percent_encode(slug, URL_SAFE)))
    }
}

fn valid_slug(s: &str) -> Result<(), &'static str> {
    if s.is_empty() || s.len() > 100 {
        return Err("Custom URL must be 1-100 characters");
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err("Custom URL may use only letters, digits, '-', '_' and '.'");
    }
    if s.starts_with('.') {
        return Err("Custom URL can't start with '.'");
    }
    if RESERVED.contains(&s.to_ascii_lowercase().as_str()) {
        return Err("That URL is reserved");
    }
    Ok(())
}

fn sanitize_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    let clean = clean.trim().trim_matches('.').trim().to_string();
    let clean: String = clean.chars().take(200).collect();
    (!clean.is_empty() && !clean.ends_with(".part")).then_some(clean)
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(unix)]
fn set_mode(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o660));
}
#[cfg(not(unix))]
fn set_mode(_: &Path) {}

fn content_disposition(path: &Path) -> HeaderValue {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "download".into());
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let v = format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", utf8_percent_encode(&name, URL_SAFE));
    HeaderValue::from_str(&v).unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

const NOT_FOUND_HTML: &str = "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'><title>Not found</title><link rel=icon type=image/png href=/favicon.png><style>body{font:16px system-ui,sans-serif;display:grid;place-items:center;min-height:90vh;color:#555;background:#fafafa}</style><p>This link doesn't exist or has expired.</p>";

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, [(X_ROBOTS, "noindex")], Html(NOT_FOUND_HTML)).into_response()
}

// ---------- public listener ----------

async fn serve_slug(State(app): State<Shared>, UrlPath(slug): UrlPath<String>, req: Request) -> Response {
    let key = slug.to_ascii_lowercase();
    let link = match app.links.read().await.get(&key) {
        Some(l) if !l.disabled && l.expires.is_none_or(|e| e > now()) => l.clone(),
        _ => return not_found(),
    };
    let Some(full) = app.resolve(&link.path) else { return not_found() };

    let counts = req.method() == Method::GET
        && req
            .headers()
            .get(header::RANGE)
            .and_then(|r| r.to_str().ok())
            .is_none_or(|r| r.trim().starts_with("bytes=0-"));

    let mut res = match ServeFile::new(&full).oneshot(req).await {
        Ok(r) => r.map(Body::new),
        Err(_) => return not_found(),
    };
    if res.status().is_success() {
        let h = res.headers_mut();
        h.insert(header::CONTENT_DISPOSITION, content_disposition(&full));
        h.insert(X_ROBOTS, HeaderValue::from_static("noindex, nofollow"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-transform"));
        if counts {
            let mut links = app.links.write().await;
            if let Some(l) = links.get_mut(&key) {
                l.downloads += 1;
                l.last_download = Some(now());
            }
            let _ = app.save(&links).await;
        }
    }
    res
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn human(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut v, mut i) = (b as f64, 0);
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[i]) }
}

#[derive(Deserialize)]
struct InfoQuery {
    #[serde(default)]
    json: Option<String>,
}

async fn info(State(app): State<Shared>, UrlPath(slug): UrlPath<String>, Query(q): Query<InfoQuery>) -> Response {
    let link = match app.links.read().await.get(&slug.to_ascii_lowercase()) {
        Some(l) if !l.disabled && l.expires.is_none_or(|e| e > now()) => l.clone(),
        _ => return not_found(),
    };
    let Some(full) = app.resolve(&link.path) else { return not_found() };
    let Some((size, _)) = stat(&full) else { return not_found() };
    let name = full.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let state = hash_state(&app, &link.path, &full).await;

    if q.json.is_some() {
        let body = match &state {
            Some(HashState::Done(h)) => serde_json::json!({ "name": name, "size": size, "url": app.url(&link.slug), "md5": h.md5, "sha1": h.sha1, "sha256": h.sha256, "sha512": h.sha512 }),
            Some(HashState::Running(done)) => serde_json::json!({ "name": name, "size": size, "url": app.url(&link.slug), "hashing": true, "progress": *done as f64 / size.max(1) as f64 }),
            None => return not_found(),
        };
        return ([(X_ROBOTS, "noindex")], Json(body)).into_response();
    }

    let dl = format!("/{}", utf8_percent_encode(&link.slug, URL_SAFE));
    let (refresh, hashes) = match state {
        Some(HashState::Done(h)) => (
            String::new(),
            [("MD5", h.md5), ("SHA-1", h.sha1), ("SHA-256", h.sha256), ("SHA-512", h.sha512)]
                .iter()
                .map(|(k, v)| format!("<div class=h><span>{k}</span><code>{v}</code><button data-v=\"{v}\">Copy</button></div>"))
                .collect::<String>(),
        ),
        Some(HashState::Running(done)) => {
            let pct = done as f64 * 100.0 / size.max(1) as f64;
            (
                "<meta http-equiv=refresh content=3>".to_string(),
                format!("<p class=muted>Calculating hashes… {pct:.0}% ({} of {})</p><div class=bar><i style=\"width:{pct:.1}%\"></i></div>", human(done), human(size)),
            )
        }
        None => return not_found(),
    };
    let expires = link.expires.map(|e| format!("<span data-t={e}></span>")).unwrap_or_else(|| "Never".into());
    let html = INFO_HTML
        .replace("{refresh}", &refresh)
        .replace("{name}", &esc(&name))
        .replace("{size}", &human(size))
        .replace("{bytes}", &size.to_string())
        .replace("{expires}", &expires)
        .replace("{dl}", &dl)
        .replace("{hashes}", &hashes);
    ([(X_ROBOTS, "noindex"), (header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

const INFO_HTML: &str = include_str!("info.html");

fn public_router(app: Shared) -> Router {
    Router::new()
        .route("/", get(|| async { not_found() }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/robots.txt", get(|| async { "User-agent: *\nDisallow: /\n" }))
        .route("/favicon.png", get(|| async { png(FAVICON) }))
        .route("/icon.png", get(|| async { png(ICON) }))
        .route("/info/{slug}", get(info))
        .route("/{slug}", get(serve_slug).head(serve_slug))
        .fallback(|| async { not_found() })
        .with_state(app)
}

// ---------- admin listener ----------

#[derive(Serialize)]
struct FileEntry {
    path: String,
    size: u64,
    modified: u64,
    links: Vec<String>,
}

fn walk(dir: &Path, out: &mut Vec<(PathBuf, std::fs::Metadata)>, depth: usize) {
    if depth > 12 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name.ends_with(".part") {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk(&e.path(), out, depth + 1);
        } else if let Ok(m) = e.metadata() {
            out.push((e.path(), m));
        }
    }
}

async fn list_files(State(app): State<Shared>) -> Response {
    let root = match app.files.canonicalize() {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("files folder: {e}")),
    };
    let found = tokio::task::spawn_blocking(move || {
        let mut v = Vec::new();
        walk(&root, &mut v, 0);
        v
    })
    .await
    .unwrap_or_default();
    let links = app.links.read().await;
    let mut out: Vec<FileEntry> = found
        .into_iter()
        .filter_map(|(p, m)| {
            let path = app.rel_of(&p)?;
            let modified = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
            let l = links.values().filter(|l| l.path == path).map(|l| l.slug.clone()).collect();
            Some(FileEntry { path, size: m.len(), modified, links: l })
        })
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    Json(out).into_response()
}

#[derive(Serialize)]
struct LinkOut {
    #[serde(flatten)]
    link: Link,
    url: String,
    backup_url: Option<String>,
    missing: bool,
    size: Option<u64>,
}

async fn list_links(State(app): State<Shared>) -> Response {
    let links = app.links.read().await;
    let mut v: Vec<LinkOut> = links
        .values()
        .map(|l| {
            let f = app.resolve(&l.path);
            LinkOut {
                url: app.url(&l.slug),
                backup_url: app.backup(&l.slug),
                missing: f.is_none(),
                size: f.and_then(|p| p.metadata().ok()).map(|m| m.len()),
                link: l.clone(),
            }
        })
        .collect();
    v.sort_by(|a, b| b.link.created.cmp(&a.link.created));
    Json(v).into_response()
}

#[derive(Deserialize)]
struct NewLink {
    path: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    expires_hours: Option<u64>,
}

async fn create_link(State(app): State<Shared>, Json(req): Json<NewLink>) -> Response {
    let Some(full) = app.resolve(&req.path) else { return err(StatusCode::BAD_REQUEST, "File not found") };
    let path = app.rel_of(&full).unwrap_or(req.path);
    let slug = match req.slug.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(s) => {
            if let Err(e) = valid_slug(&s) {
                return err(StatusCode::BAD_REQUEST, e);
            }
            s
        }
        None => uuid::Uuid::new_v4().to_string(),
    };
    let key = slug.to_ascii_lowercase();
    let mut links = app.links.write().await;
    if links.contains_key(&key) {
        return err(StatusCode::CONFLICT, "That URL is already taken");
    }
    let link = Link {
        slug: slug.clone(),
        path,
        created: now(),
        expires: req.expires_hours.filter(|h| *h > 0).map(|h| now() + h * 3600),
        downloads: 0,
        last_download: None,
        disabled: false,
    };
    links.insert(key, link.clone());
    let _ = hash_state(&app, &link.path, &full).await;
    if let Err(e) = app.save(&links).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("save failed: {e}"));
    }
    (StatusCode::CREATED, Json(LinkOut { url: app.url(&slug), backup_url: app.backup(&slug), missing: false, size: full.metadata().ok().map(|m| m.len()), link })).into_response()
}

#[derive(Deserialize)]
struct PatchLink {
    #[serde(default)]
    disabled: Option<bool>,
    /// 0 clears the expiry.
    #[serde(default)]
    expires_hours: Option<u64>,
}

async fn patch_link(State(app): State<Shared>, UrlPath(slug): UrlPath<String>, Json(p): Json<PatchLink>) -> Response {
    let mut links = app.links.write().await;
    let Some(l) = links.get_mut(&slug.to_ascii_lowercase()) else { return err(StatusCode::NOT_FOUND, "No such link") };
    if let Some(d) = p.disabled {
        l.disabled = d;
    }
    if let Some(h) = p.expires_hours {
        l.expires = (h > 0).then(|| now() + h * 3600);
    }
    let _ = app.save(&links).await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct DelQuery {
    #[serde(default)]
    delete_file: Option<u8>,
}

async fn delete_link(State(app): State<Shared>, UrlPath(slug): UrlPath<String>, Query(q): Query<DelQuery>) -> Response {
    let mut links = app.links.write().await;
    let Some(l) = links.remove(&slug.to_ascii_lowercase()) else { return err(StatusCode::NOT_FOUND, "No such link") };
    if q.delete_file == Some(1) {
        if let Some(full) = app.resolve(&l.path) {
            let _ = tokio::fs::remove_file(full).await;
        }
        links.retain(|_, o| o.path != l.path);
    }
    let _ = app.save(&links).await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct PathQuery {
    path: String,
}

async fn delete_file(State(app): State<Shared>, Query(q): Query<PathQuery>) -> Response {
    let Some(full) = app.resolve(&q.path) else { return err(StatusCode::NOT_FOUND, "File not found") };
    let rel = app.rel_of(&full).unwrap_or(q.path);
    if let Err(e) = tokio::fs::remove_file(&full).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let mut links = app.links.write().await;
    links.retain(|_, l| l.path != rel);
    let _ = app.save(&links).await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    #[serde(default)]
    dir: String,
}

async fn upload(State(app): State<Shared>, Query(q): Query<UploadQuery>, body: Body) -> Response {
    let Some(name) = sanitize_name(&q.name) else { return err(StatusCode::BAD_REQUEST, "Bad file name") };
    let Some(dir) = app.resolve_dir(q.dir.trim_matches('/')) else { return err(StatusCode::BAD_REQUEST, "Folder not found") };
    let part = dir.join(format!(".{}.{}.part", name, uuid::Uuid::new_v4().simple()));

    let result: std::io::Result<()> = async {
        let mut f = tokio::io::BufWriter::with_capacity(1 << 20, tokio::fs::File::create(&part).await?);
        let mut stream = body.into_data_stream().map_err(std::io::Error::other);
        while let Some(chunk) = stream.try_next().await? {
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        f.into_inner().sync_all().await
    }
    .await;
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&part).await;
        return err(StatusCode::BAD_REQUEST, format!("Upload failed: {e}"));
    }
    let dest = unique_path(&dir, &name);
    if let Err(e) = tokio::fs::rename(&part, &dest).await {
        let _ = tokio::fs::remove_file(&part).await;
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    set_mode(&dest);
    Json(serde_json::json!({ "path": app.rel_of(&dest) })).into_response()
}

async fn me(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let user = headers.get("remote-user").and_then(|v| v.to_str().ok()).unwrap_or("");
    Json(serde_json::json!({ "user": user, "public_url": app.public_url })).into_response()
}

async fn index() -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(INDEX_HTML)).into_response()
}

fn admin_router(app: Shared) -> Router {
    Router::new()
        .route("/", get(|| async { Redirect::to("/admin/") }))
        .route("/admin", get(|| async { Redirect::to("/admin/") }))
        .route("/admin/", get(index))
        .route("/favicon.png", get(|| async { png(FAVICON) }))
        .route("/icon.png", get(|| async { png(ICON) }))
        .route("/api/me", get(me))
        .route("/api/files", get(list_files).delete(delete_file))
        .route("/api/links", get(list_links).post(create_link))
        .route("/api/links/{slug}", axum::routing::patch(patch_link).delete(delete_link))
        .route("/api/upload", post(upload).layer(DefaultBodyLimit::disable()))
        .with_state(app)
}

#[tokio::main]
async fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let files = PathBuf::from(env("FOXDROP_FILES", "/files"));
    let data = PathBuf::from(env("FOXDROP_DATA", "/data"));
    std::fs::create_dir_all(&data).expect("create data dir");
    let db = data.join("links.json");
    let links: BTreeMap<String, Link> = match std::fs::read(&db) {
        Ok(b) => serde_json::from_slice::<Vec<Link>>(&b)
            .expect("links.json is corrupt")
            .into_iter()
            .map(|l| (l.slug.to_ascii_lowercase(), l))
            .collect(),
        Err(_) => BTreeMap::new(),
    };
    let hash_db = data.join("hashes.json");
    let hashes: BTreeMap<String, Hashes> = std::fs::read(&hash_db).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let app = Arc::new(App {
        files,
        db,
        hash_db,
        public_url: env("FOXDROP_PUBLIC_URL", "http://localhost:8080"),
        backup_url: std::env::var("FOXDROP_BACKUP_URL").ok().filter(|s| !s.is_empty()),
        links: RwLock::new(links),
        hashes: RwLock::new(hashes),
        hashing: Default::default(),
        hash_slot: tokio::sync::Semaphore::new(1),
    });
    // Hash every linked file in the background so /info is ready when someone opens it.
    {
        let paths: Vec<String> = app.links.read().await.values().map(|l| l.path.clone()).collect();
        for p in paths {
            if let Some(full) = app.resolve(&p) {
                let _ = hash_state(&app, &p, &full).await;
            }
        }
    }

    let public_addr = env("FOXDROP_PUBLIC_ADDR", "0.0.0.0:8080");
    let admin_addr = env("FOXDROP_ADMIN_ADDR", "0.0.0.0:8081");
    let pl = tokio::net::TcpListener::bind(&public_addr).await.expect("bind public");
    let al = tokio::net::TcpListener::bind(&admin_addr).await.expect("bind admin");
    println!("foxdrop: public on {public_addr}, admin on {admin_addr}");
    let p = axum::serve(pl, public_router(app.clone()));
    let a = axum::serve(al, admin_router(app));
    tokio::select! {
        r = p => r.expect("public server"),
        r = a => r.expect("admin server"),
        _ = tokio::signal::ctrl_c() => {},
    }
}
