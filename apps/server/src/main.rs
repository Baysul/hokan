mod storage;

use std::{
    collections::HashMap,
    convert::Infallible,
    env,
    fmt::Write as _,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{FromRef, Path, State},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use bytes::BytesMut;
use futures_util::{StreamExt, stream};
use serde::Serialize;
use tokio::sync::Semaphore;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

// Room for 5 GiB of plaintext plus the browser's encryption overhead.
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024 + 1024 * 1024;
// Uploads arrive in parts of this size, except the last. S3 requires parts of at least
// 5 MiB, and R2 requires them to be the same size.
const PART_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_MEMORY_LIMIT: u64 = 6 * 1024 * 1024 * 1024;
const DEFAULT_DAILY_UPLOAD_LIMIT: u64 = 50 * 1024 * 1024 * 1024;
// Parts are held in memory while they are stored, so this also bounds memory use.
const MAX_CONCURRENT_PARTS: usize = 8;
const PART_PERMIT_WAIT: Duration = Duration::from_secs(30);
const MAX_UPLOADS_PER_DAY: usize = 240;
const UPLOAD_WINDOW: Duration = Duration::from_hours(24);
// An interrupted upload can be resumed until this long after it started.
const UPLOAD_SESSION_LIFETIME: Duration = Duration::from_hours(24);
const PART_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PART_BODY_TIMEOUT: Duration = Duration::from_mins(30);
const STORAGE_TIMEOUT: Duration = Duration::from_secs(60);
const PART_STORAGE_TIMEOUT: Duration = Duration::from_mins(5);
const DEFAULT_LIFETIME: Duration = Duration::from_hours(24);
const MIN_LIFETIME: Duration = Duration::from_hours(1);
// A file is deleted once it goes this long without a download, even if its link never expires.
const IDLE_LIFETIME: Duration = Duration::from_hours(7 * 24);
const EXPIRY_HEADER: &str = "x-hokan-expiry-seconds";
const LENGTH_HEADER: &str = "x-hokan-upload-length";
const ALLOWED_EXPIRY_SECONDS: [u64; 3] = [3_600, 86_400, 604_800];

type SharedStore = Arc<Mutex<MemoryStore>>;

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

// Identifies an upload in progress by the file's ID, the upload's length, and a secret that
// only the uploader knows. For cloud storage, the secret is the multipart upload ID.
struct UploadToken {
    id: Uuid,
    length: u64,
    secret: String,
}

impl UploadToken {
    fn parse(value: &str) -> Option<Self> {
        let mut fields = value.splitn(3, '.');
        let id = Uuid::try_parse(fields.next()?).ok()?;
        let length = fields.next()?.parse().ok()?;
        let secret = fields.next()?;
        (!secret.is_empty()
            && secret.len() <= 2048
            && secret.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| Self {
            id,
            length,
            secret: secret.to_owned(),
        })
    }
}

impl std::fmt::Display for UploadToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.id, self.length, self.secret)
    }
}

fn hex_encode(value: &str) -> String {
    value.bytes().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

fn hex_decode(value: &str) -> Option<String> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(value.get(index..index + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

// The size part `number` must have. Parts are numbered from 1 and all but the last are full.
fn part_size(length: u64, number: u32, part_bytes: u64) -> Option<u64> {
    let start = u64::from(number.checked_sub(1)?) * part_bytes;
    (start < length).then(|| (length - start).min(part_bytes))
}

fn part_count(length: u64, part_bytes: u64) -> u32 {
    u32::try_from(length.div_ceil(part_bytes)).unwrap_or(u32::MAX)
}

struct MemoryStore {
    files: HashMap<Uuid, StoredFile>,
    uploads: HashMap<Uuid, MemoryUpload>,
    // Bytes held by stored files, plus the full length of each upload in progress.
    used: u64,
    limit: u64,
}

struct StoredFile {
    parts: Vec<Bytes>,
    size: u64,
    lifetime: Option<Duration>,
    expires_at: Option<Instant>,
    // When the file was last downloaded, or published if it has not been downloaded yet.
    last_download: Instant,
}

struct MemoryUpload {
    secret: String,
    length: u64,
    lifetime: Option<Duration>,
    started: Instant,
    parts: HashMap<u32, Bytes>,
}

impl MemoryStore {
    fn shared(limit: u64) -> SharedStore {
        Arc::new(Mutex::new(Self {
            files: HashMap::new(),
            uploads: HashMap::new(),
            used: 0,
            limit,
        }))
    }

    fn remove_expired(&mut self) {
        let now = Instant::now();
        let mut freed = 0;
        self.files.retain(|_, file| {
            let keep = file.expires_at.is_none_or(|expires| expires > now)
                && now.duration_since(file.last_download) < IDLE_LIFETIME;
            if !keep {
                freed += file.size;
            }
            keep
        });
        self.uploads.retain(|_, upload| {
            let keep = now.duration_since(upload.started) < UPLOAD_SESSION_LIFETIME;
            if !keep {
                freed += upload.length;
            }
            keep
        });
        self.used -= freed;
    }

    // Space for the whole upload is set aside now, so it cannot run out of room part way.
    fn create(
        &mut self,
        id: Uuid,
        length: u64,
        lifetime: Option<Duration>,
    ) -> Result<String, UploadError> {
        self.remove_expired();
        if length > self.limit - self.used {
            return Err(UploadError::Full);
        }
        self.used += length;
        let secret = Uuid::new_v4().simple().to_string();
        self.uploads.insert(
            id,
            MemoryUpload {
                secret: secret.clone(),
                length,
                lifetime,
                started: Instant::now(),
                parts: HashMap::new(),
            },
        );
        Ok(secret)
    }

    fn upload(&mut self, token: &UploadToken) -> Result<&mut MemoryUpload, UploadError> {
        self.remove_expired();
        self.uploads
            .get_mut(&token.id)
            .filter(|upload| upload.secret == token.secret && upload.length == token.length)
            .ok_or(UploadError::NotFound)
    }

    fn put_part(
        &mut self,
        token: &UploadToken,
        number: u32,
        bytes: Bytes,
    ) -> Result<(), UploadError> {
        self.upload(token)?.parts.insert(number, bytes);
        Ok(())
    }

    fn received(&mut self, token: &UploadToken) -> Result<Vec<u32>, UploadError> {
        let mut received: Vec<u32> = self.upload(token)?.parts.keys().copied().collect();
        received.sort_unstable();
        Ok(received)
    }

    fn complete(
        &mut self,
        token: &UploadToken,
        part_bytes: u64,
    ) -> Result<Option<Duration>, UploadError> {
        let Ok(upload) = self.upload(token) else {
            // A completed upload stays complete, so the uploader can safely ask again.
            return self
                .files
                .get(&token.id)
                .map(|file| file.lifetime)
                .ok_or(UploadError::NotFound);
        };
        // Every part was checked for its exact size when it arrived.
        if u32::try_from(upload.parts.len()) != Ok(part_count(upload.length, part_bytes)) {
            return Err(UploadError::Incomplete);
        }
        let upload = self
            .uploads
            .remove(&token.id)
            .expect("upload was just found");
        let mut parts: Vec<(u32, Bytes)> = upload.parts.into_iter().collect();
        parts.sort_unstable_by_key(|(number, _)| *number);
        self.files.insert(
            token.id,
            StoredFile {
                parts: parts.into_iter().map(|(_, bytes)| bytes).collect(),
                size: upload.length,
                lifetime: upload.lifetime,
                expires_at: upload.lifetime.map(|lifetime| Instant::now() + lifetime),
                last_download: Instant::now(),
            },
        );
        Ok(upload.lifetime)
    }

    fn abort(&mut self, token: &UploadToken) {
        if self.upload(token).is_ok()
            && let Some(upload) = self.uploads.remove(&token.id)
        {
            self.used -= upload.length;
        }
    }
}

enum UploadError {
    NotFound,
    Full,
    Incomplete,
    Unavailable,
}

impl UploadError {
    fn unavailable(error: impl std::fmt::Display) -> Self {
        eprintln!("Cloud storage failed: {error}");
        Self::Unavailable
    }
}

impl From<storage::CloudError> for UploadError {
    fn from(error: storage::CloudError) -> Self {
        match error {
            storage::CloudError::NotFound => Self::NotFound,
            storage::CloudError::Failed(error) => Self::unavailable(error),
        }
    }
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "Upload not found"),
            Self::Full => (StatusCode::INSUFFICIENT_STORAGE, "Storage capacity reached"),
            Self::Incomplete => (StatusCode::CONFLICT, "Upload is missing parts"),
            Self::Unavailable => (StatusCode::BAD_GATEWAY, "Storage service unavailable"),
        }
        .into_response()
    }
}

// S3 requests are large futures, so they are kept on the heap.
async fn cloud<T>(
    limit: Duration,
    operation: impl Future<Output = Result<T, storage::CloudError>>,
) -> Result<T, UploadError> {
    tokio::time::timeout(limit, Box::pin(operation))
        .await
        .unwrap_or_else(|_| {
            Err(storage::CloudError::Failed(
                "Cloud storage timed out".into(),
            ))
        })
        .map_err(UploadError::from)
}

struct UploadBudget {
    started: Instant,
    uploads: usize,
    bytes: u64,
    max_bytes: u64,
}

impl UploadBudget {
    fn new(max_bytes: u64) -> Self {
        Self {
            started: Instant::now(),
            uploads: 0,
            bytes: 0,
            max_bytes,
        }
    }

    fn refresh(&mut self) {
        if self.started.elapsed() >= UPLOAD_WINDOW {
            *self = Self::new(self.max_bytes);
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

    fn has_room(&mut self, bytes: u64) -> bool {
        self.refresh();
        bytes <= self.max_bytes - self.bytes
    }

    fn allow_bytes(&mut self, bytes: u64) -> bool {
        if !self.has_room(bytes) {
            return false;
        }
        self.bytes += bytes;
        true
    }

    fn limit_reached(&self) -> Response {
        let retry_after = UPLOAD_WINDOW.saturating_sub(self.started.elapsed());
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                header::RETRY_AFTER,
                retry_after.as_secs().max(1).to_string(),
            )],
            "Daily upload limit reached",
        )
            .into_response()
    }
}

struct UploadGate {
    part_permits: Arc<Semaphore>,
    part_wait: Duration,
    budget: Mutex<UploadBudget>,
    max_file_bytes: u64,
    part_bytes: u64,
}

impl UploadGate {
    fn new(daily_upload_limit: u64) -> Self {
        Self {
            part_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_PARTS)),
            part_wait: PART_PERMIT_WAIT,
            budget: Mutex::new(UploadBudget::new(daily_upload_limit)),
            max_file_bytes: MAX_FILE_BYTES,
            part_bytes: PART_BYTES,
        }
    }
}

#[derive(Clone)]
struct AppState {
    store: Store,
    gate: Arc<UploadGate>,
}

impl FromRef<AppState> for Store {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

#[derive(Serialize)]
struct CreatedUpload {
    upload: String,
    part_size: u64,
}

#[derive(Serialize)]
struct UploadStatus {
    part_size: u64,
    received: Vec<u32>,
}

#[derive(Serialize)]
struct UploadResponse {
    id: Uuid,
    expires_in_seconds: Option<u64>,
}

type Rejection = (StatusCode, &'static str);

fn requested_lifetime(headers: &HeaderMap) -> Result<Option<Duration>, Rejection> {
    match headers.get(EXPIRY_HEADER) {
        None => Ok(Some(DEFAULT_LIFETIME)),
        Some(value) => match value.to_str().ok().and_then(|s| s.parse::<u64>().ok()) {
            Some(0) => Ok(None),
            Some(seconds) if ALLOWED_EXPIRY_SECONDS.contains(&seconds) => {
                Ok(Some(Duration::from_secs(seconds)))
            }
            _ => Err((StatusCode::BAD_REQUEST, "Invalid expiration")),
        },
    }
}

// Starts an upload. Browsers cannot send the length header from a cross-site HTML form.
async fn create_upload(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let lifetime = match requested_lifetime(&headers) {
        Ok(lifetime) => lifetime,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(length) = headers
        .get(LENGTH_HEADER)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        .filter(|length| *length > 0)
    else {
        return (StatusCode::BAD_REQUEST, "Expected the upload length").into_response();
    };
    let gate = &state.gate;
    if length > gate.max_file_bytes {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    {
        let mut budget = gate.budget.lock().expect("upload budget lock poisoned");
        if !budget.has_room(length) || !budget.allow_attempt() {
            return budget.limit_reached();
        }
    }
    let id = Uuid::new_v4();
    let secret = match &state.store {
        Store::Memory(store) => store
            .lock()
            .expect("file store lock poisoned")
            .create(id, length, lifetime),
        Store::Cloud(store) => cloud(STORAGE_TIMEOUT, store.create_upload(id, lifetime))
            .await
            .map(|upload_id| hex_encode(&upload_id)),
    };
    match secret {
        Ok(secret) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(CreatedUpload {
                upload: UploadToken { id, length, secret }.to_string(),
                part_size: gate.part_bytes,
            }),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn read_part(body: Body, expected: u64) -> Result<Bytes, Rejection> {
    let wrong_size = || (StatusCode::BAD_REQUEST, "Part has the wrong size");
    let deadline = tokio::time::Instant::now() + PART_BODY_TIMEOUT;
    let mut stream = body.into_data_stream();
    let mut part = BytesMut::with_capacity(usize::try_from(expected).map_err(|_| wrong_size())?);
    loop {
        let idle = tokio::time::Instant::now() + PART_IDLE_TIMEOUT;
        match tokio::time::timeout_at(idle.min(deadline), stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                if (part.len() + bytes.len()) as u64 > expected {
                    return Err(wrong_size());
                }
                part.extend_from_slice(&bytes);
            }
            Ok(Some(Err(_))) => {
                return Err((StatusCode::BAD_REQUEST, "Upload interrupted"));
            }
            Ok(None) => break,
            Err(_) => return Err((StatusCode::REQUEST_TIMEOUT, "Upload timed out")),
        }
    }
    if part.len() as u64 != expected {
        return Err(wrong_size());
    }
    Ok(part.freeze())
}

// Stores one part. Sending a part again replaces it, so a failed request can simply be repeated.
async fn put_part(
    State(state): State<AppState>,
    Path((token, number)): Path<(String, u32)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if headers.get(header::CONTENT_TYPE).is_none_or(|value| {
        !value
            .to_str()
            .is_ok_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
    }) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected application/octet-stream",
        )
            .into_response();
    }
    let gate = &state.gate;
    let Some(token) =
        UploadToken::parse(&token).filter(|token| token.length <= gate.max_file_bytes)
    else {
        return UploadError::NotFound.into_response();
    };
    let Some(expected) = part_size(token.length, number, gate.part_bytes) else {
        return (StatusCode::BAD_REQUEST, "Invalid part number").into_response();
    };
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        .is_some_and(|length| length != expected)
    {
        return (StatusCode::BAD_REQUEST, "Part has the wrong size").into_response();
    }
    let Ok(Ok(_permit)) =
        tokio::time::timeout(gate.part_wait, gate.part_permits.clone().acquire_owned()).await
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            "Too many uploads in progress",
        )
            .into_response();
    };
    let bytes = match read_part(body, expected).await {
        Ok(bytes) => bytes,
        Err(rejection) => return rejection.into_response(),
    };
    {
        let mut budget = gate.budget.lock().expect("upload budget lock poisoned");
        if !budget.allow_bytes(expected) {
            return budget.limit_reached();
        }
    }
    let stored = match &state.store {
        Store::Memory(store) => store
            .lock()
            .expect("file store lock poisoned")
            .put_part(&token, number, bytes),
        Store::Cloud(store) => match hex_decode(&token.secret) {
            Some(upload_id) => {
                cloud(
                    PART_STORAGE_TIMEOUT,
                    store.upload_part(token.id, &upload_id, number, bytes),
                )
                .await
            }
            None => Err(UploadError::NotFound),
        },
    };
    match stored {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}

// Lists the parts that have arrived, so an interrupted upload can send only the rest.
async fn upload_status(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let part_bytes = state.gate.part_bytes;
    let Some(token) = UploadToken::parse(&token) else {
        return UploadError::NotFound.into_response();
    };
    let received = match &state.store {
        Store::Memory(store) => store
            .lock()
            .expect("file store lock poisoned")
            .received(&token),
        Store::Cloud(store) => match hex_decode(&token.secret) {
            Some(upload_id) => cloud(STORAGE_TIMEOUT, store.list_parts(token.id, &upload_id))
                .await
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|part| {
                            part_size(token.length, part.number, part_bytes) == Some(part.size)
                        })
                        .map(|part| part.number)
                        .collect()
                }),
            None => Err(UploadError::NotFound),
        },
    };
    match received {
        Ok(received) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(UploadStatus {
                part_size: part_bytes,
                received,
            }),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn complete_cloud(
    store: &storage::CloudStore,
    token: &UploadToken,
    part_bytes: u64,
) -> Result<Option<Duration>, UploadError> {
    let upload_id = hex_decode(&token.secret).ok_or(UploadError::NotFound)?;
    match cloud(STORAGE_TIMEOUT, store.list_parts(token.id, &upload_id)).await {
        Ok(parts) => {
            let parts: Vec<storage::Part> = parts
                .into_iter()
                .filter(|part| part_size(token.length, part.number, part_bytes) == Some(part.size))
                .collect();
            if u32::try_from(parts.len()) != Ok(part_count(token.length, part_bytes)) {
                return Err(UploadError::Incomplete);
            }
            match cloud(
                PART_STORAGE_TIMEOUT,
                store.complete_upload(token.id, &upload_id, &parts),
            )
            .await
            {
                // Not found means that an earlier request completed it.
                Ok(()) | Err(UploadError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        // The upload may already be complete, with only the reply lost.
        Err(UploadError::NotFound) => {}
        Err(error) => return Err(error),
    }
    cloud(STORAGE_TIMEOUT, store.publish(token.id)).await
}

// Finishes an upload and publishes the file. Repeating this request is safe.
async fn complete_upload(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let part_bytes = state.gate.part_bytes;
    let Some(token) = UploadToken::parse(&token) else {
        return UploadError::NotFound.into_response();
    };
    let lifetime = match &state.store {
        Store::Memory(store) => store
            .lock()
            .expect("file store lock poisoned")
            .complete(&token, part_bytes),
        Store::Cloud(store) => complete_cloud(store, &token, part_bytes).await,
    };
    match lifetime {
        Ok(lifetime) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(UploadResponse {
                id: token.id,
                expires_in_seconds: lifetime.map(|lifetime| lifetime.as_secs()),
            }),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

// Cancels an upload in progress. It never removes a file that has been published.
async fn abort_upload(State(state): State<AppState>, Path(token): Path<String>) -> StatusCode {
    let Some(token) = UploadToken::parse(&token) else {
        return StatusCode::NO_CONTENT;
    };
    match &state.store {
        Store::Memory(store) => store
            .lock()
            .expect("file store lock poisoned")
            .abort(&token),
        Store::Cloud(store) => {
            if let Some(upload_id) = hex_decode(&token.secret)
                && let Err(UploadError::Unavailable) =
                    cloud(STORAGE_TIMEOUT, store.abort_upload(token.id, &upload_id)).await
            {
                return StatusCode::BAD_GATEWAY;
            }
        }
    }
    StatusCode::NO_CONTENT
}

// Parses a single byte range such as `bytes=100-` or `bytes=100-199`. Other forms, such as
// several ranges, are ignored and the whole file is sent, as HTTP allows.
fn byte_range(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let value = headers.get(header::RANGE)?.to_str().ok()?;
    let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
    let start = start.parse().ok()?;
    let end = if end.is_empty() {
        None
    } else {
        Some(end.parse().ok()?)
    };
    end.is_none_or(|end| end >= start).then_some((start, end))
}

#[derive(Clone, Copy)]
enum Span {
    Whole,
    Part(u64, u64),
    Unsatisfiable,
}

impl Span {
    fn new(range: Option<(u64, Option<u64>)>, size: u64) -> Self {
        match range {
            None => Self::Whole,
            Some((start, _)) if start >= size => Self::Unsatisfiable,
            Some((start, end)) => Self::Part(start, end.map_or(size - 1, |end| end.min(size - 1))),
        }
    }

    // The first and last byte to send.
    fn bounds(self, size: u64) -> (u64, u64) {
        match self {
            Self::Part(start, end) => (start, end),
            Self::Whole | Self::Unsatisfiable => (0, size - 1),
        }
    }
}

// The parts' bytes from `start` to `end`, inclusive, without copying them.
fn slice_parts(parts: &[Bytes], start: u64, end: u64) -> Vec<Bytes> {
    let mut slices = Vec::new();
    let mut offset = 0;
    for part in parts {
        let length = part.len() as u64;
        let (from, to) = (start.max(offset), (end + 1).min(offset + length));
        if from < to {
            let index = |position: u64| {
                usize::try_from(position - offset).expect("the position is within the part")
            };
            slices.push(part.slice(index(from)..index(to)));
        }
        offset += length;
        if offset > end {
            break;
        }
    }
    slices
}

async fn cloud_download(
    store: &storage::CloudStore,
    id: Uuid,
    range: Option<(u64, Option<u64>)>,
) -> Result<Option<(u64, Span, Body)>, Box<dyn std::error::Error + Send + Sync>> {
    let opened = tokio::time::timeout(STORAGE_TIMEOUT, Box::pin(store.open(id, MAX_FILE_BYTES)))
        .await
        .unwrap_or_else(|_| Err("Cloud download timed out".into()))?;
    let Some(file) = opened else {
        return Ok(None);
    };
    // The download goes ahead even if it cannot be recorded.
    if let Err(error) = tokio::time::timeout(STORAGE_TIMEOUT, Box::pin(store.touch(id)))
        .await
        .unwrap_or_else(|_| Err("Cloud storage timed out".into()))
    {
        eprintln!("Could not record a download: {error}");
    }
    let span = Span::new(range, file.size);
    if let Span::Unsatisfiable = span {
        return Ok(Some((file.size, span, Body::empty())));
    }
    let (start, end) = span.bounds(file.size);
    let body = tokio::time::timeout(STORAGE_TIMEOUT, Box::pin(store.read(&file, start, end)))
        .await
        .unwrap_or_else(|_| Err("Cloud download timed out".into()))?;
    Ok(body.map(|body| (file.size, span, body)))
}

// Sends a file, or the part of it that a Range header asks for, so that a recipient whose
// connection fails can continue from where it stopped.
async fn download(
    State(store): State<Store>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let refuse =
        |status: StatusCode| (status, [(header::CACHE_CONTROL, "no-store")]).into_response();
    let Ok(id) = Uuid::parse_str(&id) else {
        return refuse(StatusCode::NOT_FOUND);
    };
    let range = byte_range(&headers);
    let (size, span, body) = match store {
        Store::Memory(store) => {
            let mut store = store.lock().expect("file store lock poisoned");
            store.remove_expired();
            let Some(file) = store.files.get_mut(&id) else {
                return refuse(StatusCode::NOT_FOUND);
            };
            file.last_download = Instant::now();
            let span = Span::new(range, file.size);
            let (start, end) = span.bounds(file.size);
            let parts = slice_parts(&file.parts, start, end)
                .into_iter()
                .map(Ok::<_, Infallible>);
            (file.size, span, Body::from_stream(stream::iter(parts)))
        }
        Store::Cloud(store) => match cloud_download(&store, id, range).await {
            Ok(Some(found)) => found,
            Ok(None) => return refuse(StatusCode::NOT_FOUND),
            Err(error) => {
                eprintln!("Cloud download failed: {error}");
                return refuse(StatusCode::BAD_GATEWAY);
            }
        },
    };
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
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let content_range = |value: String| {
        HeaderValue::from_str(&value).expect("a content range is a valid header value")
    };
    match span {
        Span::Unsatisfiable => {
            headers.insert(
                header::CONTENT_RANGE,
                content_range(format!("bytes */{size}")),
            );
            (StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response()
        }
        Span::Whole => {
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
            (StatusCode::OK, headers, body).into_response()
        }
        Span::Part(start, end) => {
            headers.insert(
                header::CONTENT_RANGE,
                content_range(format!("bytes {start}-{end}/{size}")),
            );
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(end - start + 1));
            (StatusCode::PARTIAL_CONTENT, headers, body).into_response()
        }
    }
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

fn api(store: impl Into<Store>, gate: UploadGate) -> Router {
    Router::new()
        .route("/api/uploads", post(create_upload))
        .route(
            "/api/uploads/{token}",
            get(upload_status).delete(abort_upload),
        )
        .route("/api/uploads/{token}/parts/{number}", put(put_part))
        .route("/api/uploads/{token}/complete", post(complete_upload))
        .route("/api/files/{id}", get(download))
        .route("/api/{*path}", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(AppState {
            store: store.into(),
            gate: Arc::new(gate),
        })
}

// Accepts a byte count with an optional binary unit, such as 512MiB or 6GiB.
fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let digits = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(digits);
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return None,
    };
    number.parse::<u64>().ok()?.checked_mul(multiplier)
}

fn size_from_env(name: &str, default: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(value) => {
            parse_size(&value).ok_or_else(|| format!("{name} must be a size such as 6GiB"))
        }
        Err(_) => Ok(default),
    }
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

    let daily_upload_limit = size_from_env("HOKAN_DAILY_UPLOAD_LIMIT", DEFAULT_DAILY_UPLOAD_LIMIT)?;
    let provider = env::var("HOKAN_STORAGE").unwrap_or_else(|_| "memory".into());
    let store = match provider.as_str() {
        "memory" => Store::Memory(MemoryStore::shared(size_from_env(
            "HOKAN_MEMORY_LIMIT",
            DEFAULT_MEMORY_LIMIT,
        )?)),
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
                        .remove_expired();
                }
                Store::Cloud(store) => {
                    if let Err(error) = store
                        .cleanup(MIN_LIFETIME, IDLE_LIFETIME, UPLOAD_SESSION_LIFETIME)
                        .await
                    {
                        eprintln!("Cloud cleanup failed: {error}");
                    }
                }
            }
        }
    });
    let static_files = ServeDir::new(&assets).fallback(ServeFile::new(assets.join("index.html")));
    let app = api(store, UploadGate::new(daily_upload_limit))
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
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    pub(crate) fn small_parts() -> UploadGate {
        let mut gate = UploadGate::new(DEFAULT_DAILY_UPLOAD_LIMIT);
        gate.part_bytes = 4;
        gate
    }

    pub(crate) async fn send(
        app: &Router,
        request: Request<Body>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let response = app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        (
            parts.status,
            parts.headers,
            body.collect().await.unwrap().to_bytes(),
        )
    }

    pub(crate) fn json(body: &Bytes) -> serde_json::Value {
        serde_json::from_slice(body).unwrap()
    }

    pub(crate) fn create(length: u64, expiry: Option<&str>) -> Request<Body> {
        let mut request = Request::post("/api/uploads").header(LENGTH_HEADER, length.to_string());
        if let Some(expiry) = expiry {
            request = request.header(EXPIRY_HEADER, expiry);
        }
        request.body(Body::empty()).unwrap()
    }

    pub(crate) fn part(token: &str, number: u32, bytes: &[u8]) -> Request<Body> {
        Request::put(format!("/api/uploads/{token}/parts/{number}"))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(bytes.to_vec()))
            .unwrap()
    }

    pub(crate) fn complete(token: &str) -> Request<Body> {
        Request::post(format!("/api/uploads/{token}/complete"))
            .body(Body::empty())
            .unwrap()
    }

    pub(crate) fn status(token: &str) -> Request<Body> {
        Request::get(format!("/api/uploads/{token}"))
            .body(Body::empty())
            .unwrap()
    }

    pub(crate) fn download(id: &str) -> Request<Body> {
        Request::get(format!("/api/files/{id}"))
            .body(Body::empty())
            .unwrap()
    }

    // Starts an upload and returns its token.
    pub(crate) async fn start(app: &Router, length: u64, expiry: Option<&str>) -> String {
        let (status, _, body) = send(app, create(length, expiry)).await;
        assert_eq!(status, StatusCode::CREATED);
        json(&body)["upload"].as_str().unwrap().to_owned()
    }

    // Uploads `data` in 4-byte parts and returns the published file's ID.
    pub(crate) async fn upload(app: &Router, data: &[u8], expiry: Option<&str>) -> String {
        let token = start(app, data.len() as u64, expiry).await;
        for (index, bytes) in data.chunks(4).enumerate() {
            let number = u32::try_from(index).unwrap() + 1;
            let (status, _, _) = send(app, part(&token, number, bytes)).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        let (status, _, body) = send(app, complete(&token)).await;
        assert_eq!(status, StatusCode::CREATED);
        json(&body)["id"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn parts_can_arrive_in_any_order_repeat_and_resume() {
        let store = MemoryStore::shared(DEFAULT_MEMORY_LIMIT);
        let app = api(Arc::clone(&store), small_parts());
        let token = start(&app, 17, None).await;
        let (_, _, body) = send(&app, status(&token)).await;
        assert_eq!(json(&body)["received"], serde_json::json!([]));
        assert_eq!(json(&body)["part_size"], 4);
        for (number, bytes) in [(3, "iphe"), (1, "opaq"), (5, "t"), (2, "ue c"), (1, "opaq")] {
            let (status, _, _) = send(&app, part(&token, number, bytes.as_bytes())).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        let (_, _, body) = send(&app, status(&token)).await;
        assert_eq!(json(&body)["received"], serde_json::json!([1, 2, 3, 5]));
        let (status, _, _) = send(&app, complete(&token)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        send(&app, part(&token, 4, b"rtex")).await;
        let (status, _, body) = send(&app, complete(&token)).await;
        assert_eq!(status, StatusCode::CREATED);
        let id = json(&body)["id"].as_str().unwrap().to_owned();
        assert_eq!(json(&body)["expires_in_seconds"], 86_400);
        // Completing again, as after a lost reply, gives the same answer.
        let (status, _, body) = send(&app, complete(&token)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json(&body)["id"], id.as_str());

        let (status, headers, body) = send(&app, download(&id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers[header::CONTENT_LENGTH], "17");
        assert_eq!(headers[header::CONTENT_DISPOSITION], "attachment");
        assert_eq!(body, "opaque ciphertext");
        let uuid = Uuid::parse_str(&id).unwrap();
        store
            .lock()
            .unwrap()
            .files
            .get_mut(&uuid)
            .unwrap()
            .expires_at = Some(Instant::now());
        let (status, _, _) = send(&app, download(&id)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(store.lock().unwrap().used, 0);
    }

    #[tokio::test]
    async fn downloads_continue_from_a_byte_range() {
        let app = api(MemoryStore::shared(DEFAULT_MEMORY_LIMIT), small_parts());
        let id = upload(&app, b"opaque ciphertext", None).await;
        let ranged = |range: &str| {
            Request::get(format!("/api/files/{id}"))
                .header(header::RANGE, range)
                .body(Body::empty())
                .unwrap()
        };
        let (status, headers, body) = send(&app, download(&id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(body, "opaque ciphertext");
        for (range, content_range, expected) in [
            ("bytes=5-", "bytes 5-16/17", "e ciphertext"),
            ("bytes=5-9", "bytes 5-9/17", "e cip"),
            ("bytes=16-100", "bytes 16-16/17", "t"),
            ("bytes=0-", "bytes 0-16/17", "opaque ciphertext"),
        ] {
            let (status, headers, body) = send(&app, ranged(range)).await;
            assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{range}");
            assert_eq!(headers[header::CONTENT_RANGE], content_range);
            assert_eq!(headers[header::CONTENT_LENGTH], expected.len().to_string());
            assert_eq!(body, expected);
        }
        let (status, headers, _) = send(&app, ranged("bytes=17-")).await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes */17");
        // Forms this server does not support get the whole file.
        for range in ["bytes=-5", "bytes=1-2,4-5", "bytes=9-5", "items=1-2"] {
            let (status, _, body) = send(&app, ranged(range)).await;
            assert_eq!(status, StatusCode::OK, "{range}");
            assert_eq!(body, "opaque ciphertext");
        }
    }

    #[tokio::test]
    async fn rejects_wrong_parts_and_unknown_uploads() {
        let app = api(MemoryStore::shared(DEFAULT_MEMORY_LIMIT), small_parts());
        let token = start(&app, 10, None).await;
        for (number, bytes) in [(1, &b"abc"[..]), (3, b"abcd"), (0, b"abcd"), (4, b"ab")] {
            let (status, _, _) = send(&app, part(&token, number, bytes)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "part {number}");
        }
        let (status, _, _) = send(&app, part(&token, 3, b"ab")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let other = UploadToken::parse(&token).unwrap();
        for forged in [
            format!("{}.{}.{}", other.id, other.length, "0".repeat(32)),
            format!("{}.{}.{}", other.id, other.length + 1, other.secret),
            format!("{}.{}.{}", Uuid::new_v4(), other.length, other.secret),
            "not-a-token".to_owned(),
        ] {
            let (status, _, _) = send(&app, part(&forged, 1, b"abcd")).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = send(&app, complete(&forged)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        let request = Request::put(format!("/api/uploads/{token}/parts/1"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("abcd"))
            .unwrap();
        let (status, _, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn rejects_missing_and_oversized_lengths() {
        let mut gate = small_parts();
        gate.max_file_bytes = 8;
        let app = api(MemoryStore::shared(DEFAULT_MEMORY_LIMIT), gate);
        let request = Request::post("/api/uploads").body(Body::empty()).unwrap();
        let (status, _, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = send(&app, create(0, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = send(&app, create(9, None)).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let (status, _, _) = send(&app, download("not-an-id")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn memory_is_reserved_when_an_upload_starts_and_released_when_it_ends() {
        let store = MemoryStore::shared(16);
        let app = api(Arc::clone(&store), small_parts());
        let first = start(&app, 10, None).await;
        let (status, _, body) = send(&app, create(10, None)).await;
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(body, "Storage capacity reached");
        let request = Request::delete(format!("/api/uploads/{first}"))
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(store.lock().unwrap().used, 0);
        let second = start(&app, 10, None).await;
        let (status, _, _) = send(&app, part(&second, 1, b"abcd")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        // Abandoned uploads expire and give their space back.
        let id = UploadToken::parse(&second).unwrap().id;
        store.lock().unwrap().uploads.get_mut(&id).unwrap().started =
            Instant::now().checked_sub(UPLOAD_SESSION_LIFETIME).unwrap();
        let (status, _, _) = send(&app, part(&second, 2, b"efgh")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(store.lock().unwrap().used, 0);
    }

    #[tokio::test]
    async fn selected_expiration_is_enforced_and_invalid_values_are_rejected() {
        let store = MemoryStore::shared(DEFAULT_MEMORY_LIMIT);
        let app = api(Arc::clone(&store), small_parts());
        for seconds in ALLOWED_EXPIRY_SECONDS {
            let before = Instant::now();
            let id = upload(&app, b"ciphertext", Some(&seconds.to_string())).await;
            let store = store.lock().unwrap();
            let expires = store.files[&Uuid::parse_str(&id).unwrap()]
                .expires_at
                .unwrap();
            assert!(expires >= before + Duration::from_secs(seconds));
            assert!(expires <= Instant::now() + Duration::from_secs(seconds));
        }
        for invalid in ["3601", "604801", "abc"] {
            let (status, _, _) = send(&app, create(10, Some(invalid))).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            store.lock().unwrap().files.len(),
            ALLOWED_EXPIRY_SECONDS.len()
        );
    }

    #[tokio::test]
    async fn no_expiration_remains_accessible_during_cleanup() {
        let store = MemoryStore::shared(DEFAULT_MEMORY_LIMIT);
        let app = api(Arc::clone(&store), small_parts());
        let token = start(&app, 3, Some("0")).await;
        send(&app, part(&token, 1, b"abc")).await;
        let (_, _, body) = send(&app, complete(&token)).await;
        assert!(json(&body)["expires_in_seconds"].is_null());
        let id = json(&body)["id"].as_str().unwrap().to_owned();
        store.lock().unwrap().remove_expired();
        let (status, _, body) = send(&app, download(&id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "abc");
    }

    #[tokio::test]
    async fn files_are_deleted_after_a_week_without_downloads() {
        let store = MemoryStore::shared(DEFAULT_MEMORY_LIMIT);
        let app = api(Arc::clone(&store), small_parts());
        let kept = upload(&app, b"kept", Some("0")).await;
        let idle = upload(&app, b"idle", Some("0")).await;
        let set_last_download = |id: &str, age: Duration| {
            store
                .lock()
                .unwrap()
                .files
                .get_mut(&Uuid::parse_str(id).unwrap())
                .unwrap()
                .last_download = Instant::now().checked_sub(age).unwrap();
        };
        set_last_download(&kept, IDLE_LIFETIME.saturating_sub(Duration::from_hours(1)));
        set_last_download(&idle, IDLE_LIFETIME);
        let (status, _, _) = send(&app, download(&kept)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send(&app, download(&idle)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Downloading the kept file gave it another week.
        let store = store.lock().unwrap();
        let file = &store.files[&Uuid::parse_str(&kept).unwrap()];
        assert!(file.last_download.elapsed() < Duration::from_mins(1));
        assert_eq!(store.used, 4);
    }

    #[tokio::test]
    async fn daily_limit_is_checked_before_an_upload_starts() {
        let mut gate = small_parts();
        gate.budget = Mutex::new(UploadBudget::new(10));
        let app = api(MemoryStore::shared(DEFAULT_MEMORY_LIMIT), gate);
        let (status, headers, _) = send(&app, create(11, None)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(retry_after > 0 && retry_after <= UPLOAD_WINDOW.as_secs());
        upload(&app, b"0123456789", None).await;
        let (status, _, _) = send(&app, create(1, None)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn upload_budget_caps_attempts_and_bytes_and_resets() {
        let mut budget = UploadBudget::new(DEFAULT_DAILY_UPLOAD_LIMIT);
        for _ in 0..MAX_UPLOADS_PER_DAY {
            assert!(budget.allow_attempt());
        }
        assert!(!budget.allow_attempt());
        assert!(budget.allow_bytes(DEFAULT_DAILY_UPLOAD_LIMIT));
        assert!(!budget.has_room(1));
        assert!(!budget.allow_bytes(1));
        budget.started = Instant::now().checked_sub(UPLOAD_WINDOW).unwrap();
        assert!(budget.allow_attempt());
        assert!(budget.allow_bytes(1));
    }

    #[test]
    fn tokens_and_part_sizes() {
        let upload_id = "2~abc.DEF_ghi-jkl";
        assert_eq!(hex_decode(&hex_encode(upload_id)).unwrap(), upload_id);
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
        let id = Uuid::new_v4();
        let token = UploadToken {
            id,
            length: 10,
            secret: hex_encode(upload_id),
        };
        let parsed = UploadToken::parse(&token.to_string()).unwrap();
        assert_eq!((parsed.id, parsed.length), (id, 10));
        assert!(UploadToken::parse(&format!("{id}.10.")).is_none());
        assert!(UploadToken::parse(&format!("{id}.10.xyz")).is_none());
        assert_eq!(part_size(10, 1, 4), Some(4));
        assert_eq!(part_size(10, 3, 4), Some(2));
        assert_eq!(part_size(10, 4, 4), None);
        assert_eq!(part_size(10, 0, 4), None);
        assert_eq!(part_count(10, 4), 3);
        assert_eq!(part_count(MAX_FILE_BYTES, PART_BYTES), 321);
    }

    #[test]
    fn sizes_accept_binary_units() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("512MiB"), Some(512 << 20));
        assert_eq!(parse_size(" 6 gib "), Some(6 << 30));
        assert_eq!(parse_size("6GB"), None);
        assert_eq!(parse_size("GiB"), None);
        assert_eq!(parse_size("99999999999TiB"), None);
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
        let api = app.oneshot(download("not-found")).await.unwrap();
        assert_eq!(api.headers()[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn asks_the_client_to_retry_when_too_many_parts_are_in_progress() {
        let mut gate = small_parts();
        gate.part_permits = Arc::new(Semaphore::new(1));
        gate.part_wait = Duration::from_millis(10);
        let _held_permit = gate.part_permits.clone().acquire_owned().await.unwrap();
        let app = api(MemoryStore::shared(DEFAULT_MEMORY_LIMIT), gate);
        let token = start(&app, 4, None).await;
        let (status, headers, _) = send(&app, part(&token, 1, b"abcd")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(headers[header::RETRY_AFTER], "5");
    }
}
