mod storage;

use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use tokio::sync::Semaphore;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

// Ciphertext is bounded per upload, including when cloud storage is enabled.
const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 250 * 1024 * 1024;
const MAX_CONCURRENT_UPLOADS: usize = 4;
const MAX_UPLOADS_PER_DAY: usize = 240;
const MAX_UPLOAD_BYTES_PER_DAY: usize = 1024 * 1024 * 1024;
const UPLOAD_WINDOW: Duration = Duration::from_hours(24);
const UPLOAD_BODY_TIMEOUT: Duration = Duration::from_secs(300);
const STORAGE_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_LIFETIME: Duration = Duration::from_hours(24);
const MIN_LIFETIME: Duration = Duration::from_hours(1);
const EXPIRY_HEADER: &str = "x-hokan-expiry-seconds";
const ALLOWED_EXPIRY_SECONDS: [u64; 3] = [3_600, 86_400, 604_800];

type SharedStore = Arc<Mutex<HashMap<Uuid, StoredFile>>>;

#[derive(Clone)]
enum Store {
    Memory(SharedStore),
    Cloud(storage::CloudStore),
}

impl From<SharedStore> for Store {
    fn from(store: SharedStore) -> Self {
        Self::Memory(store)
    }
}

struct StoredFile {
    bytes: Bytes,
    expires_at: Option<Instant>,
}

struct UploadBudget {
    started: Instant,
    uploads: usize,
    bytes: usize,
}

impl UploadBudget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            uploads: 0,
            bytes: 0,
        }
    }

    fn refresh(&mut self) {
        if self.started.elapsed() >= UPLOAD_WINDOW {
            *self = Self::new();
        }
    }

    fn allow_attempt(&mut self) -> bool {
        self.refresh();
        if self.uploads >= MAX_UPLOADS_PER_DAY {
            return false;
        }
        self.uploads += 1;
        true
    }

    fn allow_bytes(&mut self, bytes: usize) -> bool {
        self.refresh();
        if bytes > MAX_UPLOAD_BYTES_PER_DAY - self.bytes {
            return false;
        }
        self.bytes += bytes;
        true
    }
}

struct UploadGate {
    permits: Arc<Semaphore>,
    budget: Mutex<UploadBudget>,
}

#[derive(Serialize)]
struct UploadResponse {
    id: Uuid,
    expires_in_seconds: Option<u64>,
}

async fn upload(State(store): State<Store>, headers: HeaderMap, bytes: Bytes) -> impl IntoResponse {
    let lifetime = match headers.get(EXPIRY_HEADER) {
        None => Some(DEFAULT_LIFETIME),
        Some(value) => match value.to_str().ok().and_then(|s| s.parse::<u64>().ok()) {
            Some(0) => None,
            Some(seconds) if ALLOWED_EXPIRY_SECONDS.contains(&seconds) => {
                Some(Duration::from_secs(seconds))
            }
            _ => return (StatusCode::BAD_REQUEST, "Invalid expiration").into_response(),
        },
    };
    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "Empty payload").into_response();
    }
    let id = Uuid::new_v4();
    match store {
        Store::Memory(store) => {
            let mut files = store.lock().expect("file store lock poisoned");
            let now = Instant::now();
            files.retain(|_, file| file.expires_at.is_none_or(|expires| expires > now));
            if files.values().map(|file| file.bytes.len()).sum::<usize>() + bytes.len()
                > MAX_TOTAL_BYTES
            {
                return (StatusCode::SERVICE_UNAVAILABLE, "Storage capacity reached")
                    .into_response();
            }
            files.insert(
                id,
                StoredFile {
                    bytes,
                    expires_at: lifetime.map(|lifetime| now + lifetime),
                },
            );
        }
        Store::Cloud(store) => {
            if let Err(error) =
                tokio::time::timeout(STORAGE_TIMEOUT, store.put(id, bytes, lifetime))
                    .await
                    .unwrap_or_else(|_| Err("Cloud upload timed out".into()))
            {
                eprintln!("Cloud upload failed: {error}");
                return (StatusCode::BAD_GATEWAY, "Storage service unavailable").into_response();
            }
        }
    }
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(UploadResponse {
            id,
            expires_in_seconds: lifetime.map(|lifetime| lifetime.as_secs()),
        }),
    )
        .into_response()
}

async fn download(State(store): State<Store>, Path(id): Path<String>) -> impl IntoResponse {
    let Ok(id) = Uuid::parse_str(&id) else {
        return (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")]).into_response();
    };
    let bytes = match store {
        Store::Memory(store) => {
            let mut files = store.lock().expect("file store lock poisoned");
            let now = Instant::now();
            files.retain(|_, file| file.expires_at.is_none_or(|expires| expires > now));
            files.get(&id).map(|file| file.bytes.clone())
        }
        Store::Cloud(store) => {
            match tokio::time::timeout(STORAGE_TIMEOUT, store.get(id, MAX_FILE_BYTES))
                .await
                .unwrap_or_else(|_| Err("Cloud download timed out".into()))
            {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("Cloud download failed: {error}");
                    return (
                        StatusCode::BAD_GATEWAY,
                        [(header::CACHE_CONTROL, "no-store")],
                    )
                        .into_response();
                }
            }
        }
    };
    match bytes {
        Some(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment"),
            );
            (headers, bytes).into_response()
        }
        None => (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")]).into_response(),
    }
}

async fn limit_uploads(
    State(gate): State<Arc<UploadGate>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|value| {
            !value
                .to_str()
                .is_ok_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
        })
    {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected application/octet-stream",
        )
            .into_response();
    }
    let Ok(_permit) = gate.permits.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many uploads in progress",
        )
            .into_response();
    };
    if !gate
        .budget
        .lock()
        .expect("upload budget lock poisoned")
        .allow_attempt()
    {
        return (StatusCode::TOO_MANY_REQUESTS, "Daily upload limit reached").into_response();
    }
    let (parts, body) = request.into_parts();
    let bytes =
        match tokio::time::timeout(UPLOAD_BODY_TIMEOUT, to_bytes(body, MAX_FILE_BYTES)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
            Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
        };
    if !gate
        .budget
        .lock()
        .expect("upload budget lock poisoned")
        .allow_bytes(bytes.len())
    {
        return (StatusCode::TOO_MANY_REQUESTS, "Daily upload limit reached").into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let no_store =
        request.uri().path().starts_with("/api/") || request.uri().path().starts_with("/share/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
    );
    if no_store {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

fn api(store: impl Into<Store>) -> Router {
    api_with_upload_limit(store, Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS)))
}

fn api_with_upload_limit(store: impl Into<Store>, permits: Arc<Semaphore>) -> Router {
    let gate = Arc::new(UploadGate {
        permits,
        budget: Mutex::new(UploadBudget::new()),
    });
    Router::new()
        .route(
            "/api/files",
            post(upload).layer(middleware::from_fn_with_state(gate, limit_uploads)),
        )
        .route("/api/files/{id}", get(download))
        .route("/api/{*path}", get(|| async { StatusCode::NOT_FOUND }))
        .layer(DefaultBodyLimit::max(MAX_FILE_BYTES))
        .with_state(store.into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let address: SocketAddr = env::var("HOKAN_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse()?;
    let assets = env::var_os("HOKAN_ASSETS_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../website/dist"),
        PathBuf::from,
    );

    if !assets.join("index.html").is_file() {
        return Err(format!(
            "No built website found at {}. Run `vp run website#build` first, or set HOKAN_ASSETS_DIR.",
            assets.display()
        ).into());
    }

    let provider = env::var("HOKAN_STORAGE").unwrap_or_else(|_| "memory".into());
    let store = match provider.as_str() {
        "memory" => Store::Memory(Arc::new(Mutex::new(HashMap::new()))),
        "s3" | "r2" | "b2" => Store::Cloud(storage::CloudStore::from_env(&provider)?),
        _ => return Err("HOKAN_STORAGE must be memory, s3, r2, or b2".into()),
    };
    let cleanup_store = store.clone();
    tokio::spawn(async move {
        let period = match &cleanup_store {
            Store::Memory(_) => 60,
            Store::Cloud(_) => 60 * 60,
        };
        let mut interval = tokio::time::interval(Duration::from_secs(period));
        loop {
            interval.tick().await;
            match &cleanup_store {
                Store::Memory(store) => {
                    store
                        .lock()
                        .expect("file store lock poisoned")
                        .retain(|_, file| {
                            file.expires_at
                                .is_none_or(|expires| expires > Instant::now())
                        });
                }
                Store::Cloud(store) => {
                    if let Err(error) = store.cleanup(MIN_LIFETIME).await {
                        eprintln!("Cloud cleanup failed: {error}");
                    }
                }
            }
        }
    });
    let static_files = ServeDir::new(&assets).fallback(ServeFile::new(assets.join("index.html")));
    let app = api(store)
        .route("/health", get(|| async { "ok" }))
        .fallback_service(static_files)
        .layer(middleware::from_fn(security_headers));

    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("Hokan server listening on http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn encrypted_bytes_round_trip_and_expired_files_are_inaccessible() {
        let store = Arc::new(Mutex::new(HashMap::new()));
        let app = api(Arc::clone(&store));
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/files")
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from("opaque ciphertext"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let id = *store.lock().unwrap().keys().next().unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/files/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "opaque ciphertext"
        );
        store.lock().unwrap().get_mut(&id).unwrap().expires_at = Some(Instant::now());
        let response = app
            .oneshot(
                Request::get(format!("/api/files/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn selected_expiration_is_enforced_and_invalid_values_are_rejected() {
        let store = Arc::new(Mutex::new(HashMap::new()));
        let app = api(Arc::clone(&store));
        for seconds in ALLOWED_EXPIRY_SECONDS {
            let before = Instant::now();
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/files")
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .header(EXPIRY_HEADER, seconds.to_string())
                        .body(Body::from("ciphertext"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body = std::str::from_utf8(&body).unwrap();
            assert!(body.contains(&format!("\"expires_in_seconds\":{seconds}")));
            let files = store.lock().unwrap();
            assert!(files.values().any(|file| {
                file.expires_at.is_some_and(|expires| {
                    expires >= before + Duration::from_secs(seconds)
                        && expires <= Instant::now() + Duration::from_secs(seconds)
                })
            }));
        }
        for invalid in ["3601", "604801", "abc"] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/files")
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .header(EXPIRY_HEADER, invalid)
                        .body(Body::from("ciphertext"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert_eq!(store.lock().unwrap().len(), ALLOWED_EXPIRY_SECONDS.len());
    }

    #[tokio::test]
    async fn no_expiration_remains_accessible_during_cleanup() {
        let store = Arc::new(Mutex::new(HashMap::new()));
        let app = api(Arc::clone(&store));
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/files")
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(EXPIRY_HEADER, "0")
                    .body(Body::from("ciphertext"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains("\"expires_in_seconds\":null")
        );
        let id = *store.lock().unwrap().keys().next().unwrap();
        assert!(store.lock().unwrap().get(&id).unwrap().expires_at.is_none());
        let response = app
            .oneshot(
                Request::get(format!("/api/files/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "ciphertext"
        );
    }

    #[tokio::test]
    async fn rejects_empty_oversized_and_invalid_requests() {
        let app = api(Arc::new(Mutex::new(HashMap::new())));
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/files")
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/files")
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from(vec![0u8; MAX_FILE_BYTES + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let response = app
            .oneshot(
                Request::get("/api/files/not-an-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_browser_form_uploads_without_octet_stream_content_type() {
        let app = api(Arc::new(Mutex::new(HashMap::new())));
        for content_type in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let mut request = Request::post("/api/files");
            if let Some(content_type) = content_type {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::from("form data")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }
    }

    #[test]
    fn upload_budget_caps_attempts_and_bytes_and_resets() {
        let mut budget = UploadBudget::new();
        for _ in 0..MAX_UPLOADS_PER_DAY {
            assert!(budget.allow_attempt());
        }
        assert!(!budget.allow_attempt());
        assert!(budget.allow_bytes(MAX_UPLOAD_BYTES_PER_DAY));
        assert!(!budget.allow_bytes(1));
        budget.started = Instant::now().checked_sub(UPLOAD_WINDOW).unwrap();
        assert!(budget.allow_attempt());
        assert!(budget.allow_bytes(1));
    }

    #[tokio::test]
    async fn security_headers_cover_html_and_api_responses() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(middleware::from_fn(security_headers));
        let html = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(html.headers()[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(html.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(html.headers()["x-frame-options"], "DENY");
        assert!(
            html.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        let api = app
            .oneshot(
                Request::get("/api/files/not-found")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn rejects_upload_before_reading_body_when_at_concurrency_limit() {
        let permits = Arc::new(Semaphore::new(1));
        let _held_permit = permits.clone().acquire_owned().await.unwrap();
        let app = api_with_upload_limit(Arc::new(Mutex::new(HashMap::new())), permits);
        let response = app
            .oneshot(
                Request::post("/api/files")
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .body(Body::from("ciphertext"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
