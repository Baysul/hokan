use std::{
    collections::{HashMap, HashSet},
    env,
    error::Error,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, Credentials, Region, RequestChecksumCalculation, http::HttpResponse,
    },
    error::SdkError,
    operation::{get_object::GetObjectOutput, head_object::HeadObjectOutput},
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart, Object},
};
use axum::body::{Body, Bytes};
use axum::http::Uri;
use uuid::Uuid;

// A published file has a marker at hokan/{id}. The marker is written when the upload completes,
// so the link's lifetime starts then. Its ciphertext is uploaded in parts to hokan/data/{id}.
// Files from before resumable uploads keep their ciphertext in the marker itself. An empty object
// at hokan/accessed/{id} was last modified when the file was last downloaded, or when cleanup
// first found it.
const PREFIX: &str = "hokan/";
const DATA_PREFIX: &str = "hokan/data/";
const ACCESSED_PREFIX: &str = "hokan/accessed/";
const EXPIRES_METADATA: &str = "expires-at";
const LIFETIME_METADATA: &str = "lifetime";
const READ_TIMEOUT: Duration = Duration::from_secs(60);
type StorageError = Box<dyn Error + Send + Sync>;

pub enum CloudError {
    NotFound,
    Failed(StorageError),
}

impl CloudError {
    fn failed(message: &str) -> Self {
        Self::Failed(message.into())
    }
}

impl<E: Error + Send + Sync + 'static> From<SdkError<E, HttpResponse>> for CloudError {
    fn from(error: SdkError<E, HttpResponse>) -> Self {
        if is_not_found(&error) {
            Self::NotFound
        } else {
            Self::Failed(Box::new(error))
        }
    }
}

pub struct Part {
    pub number: u32,
    pub size: u64,
    etag: Option<String>,
}

fn marker_key(id: Uuid) -> String {
    format!("{PREFIX}{id}")
}

fn data_key(id: Uuid) -> String {
    format!("{DATA_PREFIX}{id}")
}

fn accessed_key(id: Uuid) -> String {
    format!("{ACCESSED_PREFIX}{id}")
}

fn is_not_found<E>(error: &SdkError<E, HttpResponse>) -> bool {
    error
        .raw_response()
        .is_some_and(|response| response.status().as_u16() == 404)
}

fn now_seconds() -> Result<u64, StorageError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn validate_endpoint(endpoint: &str) -> Result<(), StorageError> {
    let uri: Uri = endpoint.parse()?;
    if !uri
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
        || uri.host().is_none()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err("HOKAN_S3_ENDPOINT must be an HTTPS URL without embedded credentials".into());
    }
    Ok(())
}

fn is_expired(value: Option<&String>) -> Result<bool, StorageError> {
    match value.map(String::as_str) {
        Some("never") => Ok(false),
        Some(value) => {
            let expires = value.parse::<u64>()?;
            Ok(expires <= now_seconds()?)
        }
        None => Err("Stored object has missing expiry metadata".into()),
    }
}

fn lifetime_value(lifetime: Option<Duration>) -> String {
    lifetime.map_or_else(
        || "never".to_owned(),
        |lifetime| lifetime.as_secs().to_string(),
    )
}

fn parse_lifetime(value: Option<&String>) -> Result<Option<Duration>, CloudError> {
    match value.map(String::as_str) {
        Some("never") => Ok(None),
        Some(value) => value
            .parse()
            .map(|seconds| Some(Duration::from_secs(seconds)))
            .map_err(|_| CloudError::failed("Stored object has invalid lifetime metadata")),
        None => Err(CloudError::failed(
            "Stored object has missing lifetime metadata",
        )),
    }
}

// Streams an object's body, which must be `length` bytes long.
fn stream_object(object: GetObjectOutput, length: u64) -> Result<Body, StorageError> {
    if object.content_length() != i64::try_from(length).ok() {
        return Err("Stored object has an unexpected length".into());
    }
    let stream = futures_util::stream::try_unfold(
        (object.body, 0),
        move |(mut body, received): (ByteStream, u64)| async move {
            let next = tokio::time::timeout(READ_TIMEOUT, body.try_next())
                .await
                .map_err(|_| -> StorageError { "Cloud download stalled".into() })??;
            let Some(bytes) = next else {
                if received != length {
                    return Err("Stored object ended early".into());
                }
                return Ok(None);
            };
            let received = received + bytes.len() as u64;
            // Do not trust Content-Length alone: a misbehaving endpoint may stream more bytes.
            if received > length {
                return Err::<_, StorageError>("Stored object exceeds its length".into());
            }
            Ok(Some((bytes, (body, received))))
        },
    );
    Ok(Body::from_stream(stream))
}

// The ciphertext of a published file.
pub struct CloudFile {
    key: String,
    pub size: u64,
}

#[derive(Clone)]
pub struct CloudStore {
    client: Client,
    bucket: String,
}

impl CloudStore {
    pub fn from_env(provider: &str) -> Result<Self, StorageError> {
        let required = |name: &str| -> Result<String, StorageError> {
            match env::var(name) {
                Ok(value) if !value.trim().is_empty() => Ok(value),
                _ => Err(format!("{name} must be set for {provider} storage").into()),
            }
        };
        let bucket = required("HOKAN_S3_BUCKET")?;
        let region = required("HOKAN_S3_REGION")?;
        let access_key = required("HOKAN_S3_ACCESS_KEY_ID")?;
        let secret_key = required("HOKAN_S3_SECRET_ACCESS_KEY")?;
        let session_token = env::var("HOKAN_S3_SESSION_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let endpoint = match env::var("HOKAN_S3_ENDPOINT") {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ if provider == "s3" => None,
            _ => {
                return Err(format!("HOKAN_S3_ENDPOINT must be set for {provider} storage").into());
            }
        };
        if let Some(endpoint) = &endpoint {
            validate_endpoint(endpoint)?;
        }
        let credentials = Credentials::new(access_key, secret_key, session_token, None, "hokan");
        let mut builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region))
            .credentials_provider(credentials)
            .force_path_style(true)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired);
        if let Some(endpoint) = endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket,
        })
    }

    // Returns the multipart upload ID.
    pub async fn create_upload(
        &self,
        id: Uuid,
        lifetime: Option<Duration>,
    ) -> Result<String, CloudError> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(data_key(id))
            .content_type("application/octet-stream")
            .metadata(LIFETIME_METADATA, lifetime_value(lifetime))
            .send()
            .await?;
        Ok(created
            .upload_id()
            .ok_or_else(|| CloudError::failed("Storage did not return a multipart upload ID"))?
            .to_owned())
    }

    pub async fn upload_part(
        &self,
        id: Uuid,
        upload_id: &str,
        number: u32,
        bytes: Bytes,
    ) -> Result<(), CloudError> {
        self.client
            .upload_part()
            .bucket(&self.bucket)
            .key(data_key(id))
            .upload_id(upload_id)
            .part_number(i32::try_from(number).map_err(|_| CloudError::NotFound)?)
            .body(ByteStream::from(bytes))
            .send()
            .await?;
        Ok(())
    }

    pub async fn list_parts(&self, id: Uuid, upload_id: &str) -> Result<Vec<Part>, CloudError> {
        let mut parts = Vec::new();
        let mut marker = None;
        loop {
            let page = self
                .client
                .list_parts()
                .bucket(&self.bucket)
                .key(data_key(id))
                .upload_id(upload_id)
                .set_part_number_marker(marker)
                .send()
                .await?;
            for part in page.parts() {
                let (Some(number), Some(size)) = (
                    part.part_number()
                        .and_then(|number| u32::try_from(number).ok()),
                    part.size().and_then(|size| u64::try_from(size).ok()),
                ) else {
                    continue;
                };
                parts.push(Part {
                    number,
                    size,
                    etag: part.e_tag().map(str::to_owned),
                });
            }
            if page.is_truncated() != Some(true) {
                return Ok(parts);
            }
            marker = Some(
                page.next_part_number_marker()
                    .ok_or_else(|| CloudError::failed("Missing S3 part number marker"))?
                    .to_owned(),
            );
        }
    }

    pub async fn complete_upload(
        &self,
        id: Uuid,
        upload_id: &str,
        parts: &[Part],
    ) -> Result<(), CloudError> {
        let parts = parts
            .iter()
            .map(|part| {
                Ok(CompletedPart::builder()
                    .part_number(i32::try_from(part.number).map_err(|_| CloudError::NotFound)?)
                    .set_e_tag(part.etag.clone())
                    .build())
            })
            .collect::<Result<Vec<_>, CloudError>>()?;
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(data_key(id))
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await?;
        Ok(())
    }

    pub async fn abort_upload(&self, id: Uuid, upload_id: &str) -> Result<(), CloudError> {
        match self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(data_key(id))
            .upload_id(upload_id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => match CloudError::from(error) {
                CloudError::NotFound => Ok(()),
                CloudError::Failed(error) => Err(CloudError::Failed(error)),
            },
        }
    }

    async fn metadata(&self, key: &str) -> Result<HashMap<String, String>, CloudError> {
        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        Ok(head.metadata().cloned().unwrap_or_default())
    }

    // Writes the marker for an upload that has completed, or reads it back if it exists
    // already. Returns the lifetime chosen when the upload started.
    pub async fn publish(&self, id: Uuid) -> Result<Option<Duration>, CloudError> {
        match self.metadata(&marker_key(id)).await {
            Ok(metadata) => return parse_lifetime(metadata.get(LIFETIME_METADATA)),
            Err(CloudError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let lifetime = parse_lifetime(self.metadata(&data_key(id)).await?.get(LIFETIME_METADATA))?;
        let expires = match lifetime {
            Some(lifetime) => {
                (now_seconds().map_err(CloudError::Failed)? + lifetime.as_secs()).to_string()
            }
            None => "never".to_owned(),
        };
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(marker_key(id))
            .content_type("application/octet-stream")
            .metadata(EXPIRES_METADATA, expires)
            .metadata(LIFETIME_METADATA, lifetime_value(lifetime))
            .body(ByteStream::from_static(b""))
            .send()
            .await?;
        Ok(lifetime)
    }

    async fn head(&self, key: &str) -> Result<Option<HeadObjectOutput>, StorageError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(head) => Ok(Some(head)),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(Box::new(error)),
        }
    }

    // Finds the ciphertext of a published file that has not expired.
    pub async fn open(&self, id: Uuid, max_bytes: u64) -> Result<Option<CloudFile>, StorageError> {
        let Some(marker) = self.head(&marker_key(id)).await? else {
            return Ok(None);
        };
        if is_expired(
            marker
                .metadata()
                .and_then(|metadata| metadata.get(EXPIRES_METADATA)),
        )? {
            return Ok(None);
        }
        let (key, head) = if marker.content_length() == Some(0) {
            let key = data_key(id);
            let Some(head) = self.head(&key).await? else {
                return Ok(None);
            };
            (key, head)
        } else {
            (marker_key(id), marker)
        };
        let size = head
            .content_length()
            .and_then(|length| u64::try_from(length).ok())
            .filter(|length| *length > 0 && *length <= max_bytes)
            .ok_or("Stored object exceeds the maximum allowed size or has no content length")?;
        Ok(Some(CloudFile { key, size }))
    }

    // Streams bytes `start` to `end` of a file, inclusive.
    pub async fn read(
        &self,
        file: &CloudFile,
        start: u64,
        end: u64,
    ) -> Result<Option<Body>, StorageError> {
        let mut request = self.client.get_object().bucket(&self.bucket).key(&file.key);
        if start > 0 || end + 1 < file.size {
            request = request.range(format!("bytes={start}-{end}"));
        }
        match request.send().await {
            Ok(object) => stream_object(object, end - start + 1).map(Some),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(Box::new(error)),
        }
    }

    // Restarts the time until a file is deleted for going without downloads.
    pub async fn touch(&self, id: Uuid) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(accessed_key(id))
            .content_type("application/octet-stream")
            .body(ByteStream::from_static(b""))
            .send()
            .await?;
        Ok(())
    }

    async fn list(
        &self,
        prefix: &str,
        delimiter: Option<&str>,
    ) -> Result<Vec<Object>, StorageError> {
        let mut objects = Vec::new();
        let mut token = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_delimiter(delimiter.map(str::to_owned))
                .set_continuation_token(token)
                .send()
                .await?;
            objects.extend_from_slice(page.contents());
            if page.is_truncated() != Some(true) {
                return Ok(objects);
            }
            token = Some(
                page.next_continuation_token()
                    .ok_or("Missing S3 continuation token")?
                    .to_owned(),
            );
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        Ok(())
    }

    // The ciphertext goes first, so the marker remains to find it if this fails.
    async fn remove(&self, id: Uuid) -> Result<(), StorageError> {
        self.delete(&data_key(id)).await?;
        self.delete(&marker_key(id)).await?;
        self.delete(&accessed_key(id)).await
    }

    // Only touches objects in Hokan's namespace with a UUID name. S3 listings include each
    // object's last modification time, so cleanup still works after a server restart.
    pub async fn cleanup(
        &self,
        min_lifetime: Duration,
        idle_lifetime: Duration,
        upload_lifetime: Duration,
    ) -> Result<(), StorageError> {
        let now = now_seconds()?;
        let older_than = |time: Option<&aws_sdk_s3::primitives::DateTime>, age: Duration| {
            time.is_some_and(|time| {
                u64::try_from(time.secs())
                    .is_ok_and(|secs| secs <= now.saturating_sub(age.as_secs()))
            })
        };
        let mut accessed = HashMap::new();
        for object in self.list(ACCESSED_PREFIX, None).await? {
            if let Some(id) = object
                .key()
                .and_then(|key| key.strip_prefix(ACCESSED_PREFIX))
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                accessed.insert(id, object.last_modified().copied());
            }
        }
        let mut published = HashSet::new();
        for object in self.list(PREFIX, Some("/")).await? {
            let Some(key) = object.key() else { continue };
            let Some(id) = key
                .strip_prefix(PREFIX)
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            published.insert(id);
            match accessed.get(&id) {
                // A new file, or one from before downloads were recorded, starts its idle time now.
                None => self.touch(id).await?,
                Some(time) if older_than(time.as_ref(), idle_lifetime) => {
                    self.remove(id).await?;
                    continue;
                }
                Some(_) => {}
            }
            if !older_than(object.last_modified(), min_lifetime) {
                continue;
            }
            let head = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;
            if is_expired(
                head.metadata()
                    .and_then(|metadata| metadata.get(EXPIRES_METADATA)),
            )? {
                self.remove(id).await?;
            }
        }
        // Download records of files that were removed.
        for id in accessed.keys().filter(|id| !published.contains(id)) {
            self.delete(&accessed_key(*id)).await?;
        }
        // Ciphertext from an upload that completed but whose marker was never written.
        for object in self.list(DATA_PREFIX, None).await? {
            let Some(key) = object.key() else { continue };
            let Some(id) = key
                .strip_prefix(DATA_PREFIX)
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                continue;
            };
            if !published.contains(&id) && older_than(object.last_modified(), upload_lifetime * 2) {
                self.delete(key).await?;
            }
        }
        // Uploads that were started but never completed.
        let (mut key_marker, mut upload_id_marker) = (None, None);
        loop {
            let page = self
                .client
                .list_multipart_uploads()
                .bucket(&self.bucket)
                .prefix(DATA_PREFIX)
                .set_key_marker(key_marker)
                .set_upload_id_marker(upload_id_marker)
                .send()
                .await?;
            for upload in page.uploads() {
                if let (Some(key), Some(upload_id)) = (upload.key(), upload.upload_id())
                    && older_than(upload.initiated(), upload_lifetime)
                {
                    self.client
                        .abort_multipart_upload()
                        .bucket(&self.bucket)
                        .key(key)
                        .upload_id(upload_id)
                        .send()
                        .await?;
                }
            }
            if page.is_truncated() != Some(true) {
                return Ok(());
            }
            key_marker = page.next_key_marker().map(str::to_owned);
            upload_id_marker = page.next_upload_id_marker().map(str::to_owned);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{complete, download, json, part, send, small_parts, start, upload};
    use axum::{
        Router,
        body::to_bytes,
        extract::State,
        http::{Request, StatusCode, header},
        response::Response,
    };
    use http_body_util::BodyExt;
    use std::{
        collections::BTreeMap,
        fmt::Write as _,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct Object {
        bytes: Bytes,
        metadata: Vec<(String, String)>,
        modified: SystemTime,
    }

    struct Upload {
        key: String,
        metadata: Vec<(String, String)>,
        parts: BTreeMap<u32, Bytes>,
    }

    #[derive(Default)]
    struct Bucket {
        objects: BTreeMap<String, Object>,
        uploads: BTreeMap<String, Upload>,
    }
    type Shared = Arc<Mutex<Bucket>>;

    fn query_value<'a>(query: &'a str, name: &str) -> Option<&'a str> {
        query.split('&').find_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (key == name).then_some(value)
        })
    }

    fn reply(status: u16, body: impl Into<Body>) -> Response {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/xml")
            .body(body.into())
            .unwrap()
    }

    fn no_such_upload() -> Response {
        reply(404, "<Error><Code>NoSuchUpload</Code></Error>")
    }

    // Lists one object per page, so that pagination is exercised.
    fn list_objects(bucket: &Bucket, query: &str) -> Response {
        let decode = |value: &str| value.replace("%2F", "/");
        let prefix = query_value(query, "prefix").map(decode).unwrap_or_default();
        let delimiter = query_value(query, "delimiter").map(decode);
        let skip: usize =
            query_value(query, "continuation-token").map_or(0, |t| t.parse().unwrap());
        let objects: Vec<(&String, &Object)> = bucket
            .objects
            .iter()
            .filter(|(key, _)| {
                key.strip_prefix(&prefix).is_some_and(|rest| {
                    delimiter
                        .as_deref()
                        .is_none_or(|delimiter| !rest.contains(delimiter))
                })
            })
            .collect();
        let more = objects.len() > skip + 1;
        let mut body = format!("<ListBucketResult><IsTruncated>{more}</IsTruncated>");
        if let Some((key, object)) = objects.get(skip) {
            let modified = aws_sdk_s3::primitives::DateTime::from(object.modified)
                .fmt(aws_sdk_s3::primitives::DateTimeFormat::DateTime)
                .unwrap();
            let _ = write!(
                body,
                "<Contents><Key>{key}</Key><LastModified>{modified}</LastModified><Size>1</Size></Contents>"
            );
        }
        if more {
            let _ = write!(
                body,
                "<NextContinuationToken>{}</NextContinuationToken>",
                skip + 1
            );
        }
        body.push_str("</ListBucketResult>");
        reply(200, body)
    }

    #[allow(clippy::too_many_lines)]
    async fn mock_s3(State(bucket): State<Shared>, request: Request<Body>) -> Response {
        let method = request.method().as_str().to_owned();
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or("").to_owned();
        if path.starts_with("/error/") {
            return reply(500, "");
        }
        let Some(key) = path.strip_prefix("/test-bucket/").map(str::to_owned) else {
            return reply(404, "");
        };
        let metadata: Vec<(String, String)> = request
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((
                    name.as_str().strip_prefix("x-amz-meta-")?.to_owned(),
                    value.to_str().ok()?.to_owned(),
                ))
            })
            .collect();
        let range = request
            .headers()
            .get(header::RANGE)
            .map(|value| value.to_str().unwrap().to_owned());
        let body = to_bytes(request.into_body(), 1 << 20).await.unwrap();
        let mut bucket = bucket.lock().unwrap();
        if key.is_empty() {
            if query_value(&query, "uploads").is_none() {
                return list_objects(&bucket, &query);
            }
            let mut body = String::from(
                "<ListMultipartUploadsResult><Bucket>test-bucket</Bucket><IsTruncated>false</IsTruncated>",
            );
            for (upload_id, upload) in &bucket.uploads {
                let _ = write!(
                    body,
                    "<Upload><Key>{}</Key><UploadId>{upload_id}</UploadId><Initiated>2000-01-01T00:00:00.000Z</Initiated></Upload>",
                    upload.key
                );
            }
            body.push_str("</ListMultipartUploadsResult>");
            return reply(200, body);
        }
        match (method.as_str(), query_value(&query, "uploadId")) {
            ("POST", None) => {
                let upload_id = Uuid::new_v4().to_string();
                bucket.uploads.insert(
                    upload_id.clone(),
                    Upload {
                        key: key.clone(),
                        metadata,
                        parts: BTreeMap::new(),
                    },
                );
                reply(
                    200,
                    format!(
                        "<InitiateMultipartUploadResult><Bucket>test-bucket</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
                    ),
                )
            }
            ("PUT", Some(upload_id)) => {
                let number = query_value(&query, "partNumber").unwrap().parse().unwrap();
                let Some(upload) = bucket.uploads.get_mut(upload_id) else {
                    return no_such_upload();
                };
                upload.parts.insert(number, body);
                Response::builder()
                    .header(header::ETAG, format!("\"etag-{number}\""))
                    .body(Body::empty())
                    .unwrap()
            }
            ("GET", Some(upload_id)) => {
                let Some(upload) = bucket.uploads.get(upload_id) else {
                    return no_such_upload();
                };
                let mut body = format!(
                    "<ListPartsResult><Bucket>test-bucket</Bucket><Key>{key}</Key><UploadId>{upload_id}</UploadId><IsTruncated>false</IsTruncated>"
                );
                for (number, bytes) in &upload.parts {
                    let _ = write!(
                        body,
                        "<Part><PartNumber>{number}</PartNumber><ETag>\"etag-{number}\"</ETag><Size>{}</Size></Part>",
                        bytes.len()
                    );
                }
                body.push_str("</ListPartsResult>");
                reply(200, body)
            }
            ("POST", Some(upload_id)) => {
                let Some(upload) = bucket.uploads.remove(upload_id) else {
                    return no_such_upload();
                };
                let bytes = upload.parts.into_values().flatten().collect::<Vec<u8>>();
                bucket.objects.insert(
                    upload.key,
                    Object {
                        bytes: bytes.into(),
                        metadata: upload.metadata,
                        modified: SystemTime::now(),
                    },
                );
                reply(
                    200,
                    format!(
                        "<CompleteMultipartUploadResult><Bucket>test-bucket</Bucket><Key>{key}</Key><ETag>\"done\"</ETag></CompleteMultipartUploadResult>"
                    ),
                )
            }
            ("DELETE", Some(upload_id)) => match bucket.uploads.remove(upload_id) {
                Some(_) => reply(204, ""),
                None => no_such_upload(),
            },
            ("PUT", None) => {
                bucket.objects.insert(
                    key,
                    Object {
                        bytes: body,
                        metadata,
                        modified: SystemTime::now(),
                    },
                );
                reply(200, "")
            }
            ("GET" | "HEAD", None) => {
                let Some(object) = bucket.objects.get(&key) else {
                    return reply(404, "<Error><Code>NoSuchKey</Code></Error>");
                };
                let range = range.and_then(|range| {
                    let (start, end) = range.strip_prefix("bytes=")?.split_once('-')?;
                    Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
                });
                if let Some((start, end)) = range {
                    return Response::builder()
                        .status(206)
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{end}/{}", object.bytes.len()),
                        )
                        .header(header::CONTENT_LENGTH, end - start + 1)
                        .body(Body::from(object.bytes.slice(start..=end)))
                        .unwrap();
                }
                let mut response =
                    Response::builder().header(header::CONTENT_LENGTH, object.bytes.len());
                for (name, value) in &object.metadata {
                    response = response.header(format!("x-amz-meta-{name}"), value);
                }
                response
                    .body(if method == "HEAD" {
                        Body::empty()
                    } else {
                        Body::from(object.bytes.clone())
                    })
                    .unwrap()
            }
            ("DELETE", None) => {
                bucket.objects.remove(&key);
                reply(204, "")
            }
            _ => reply(405, ""),
        }
    }

    fn client(endpoint: &str, bucket: &str) -> CloudStore {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-west-004"))
            .credentials_provider(Credentials::new("key", "secret", None, None, "test"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .build();
        CloudStore {
            client: Client::from_conf(config),
            bucket: bucket.into(),
        }
    }

    async fn start_mock() -> (String, Shared, tokio::task::JoinHandle<()>) {
        let bucket: Shared = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(mock_s3).with_state(bucket.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (endpoint, bucket, task)
    }

    async fn read(store: &CloudStore, id: Uuid) -> Option<Bytes> {
        let file = store.open(id, 100).await.unwrap()?;
        let body = store.read(&file, 0, file.size - 1).await.unwrap()?;
        Some(body.collect().await.unwrap().to_bytes())
    }

    // Makes the objects whose keys start with `prefix` look as if they were written long ago.
    fn backdate(bucket: &Shared, prefix: &str) {
        for (key, object) in &mut bucket.lock().unwrap().objects {
            if key.starts_with(prefix) {
                object.modified = UNIX_EPOCH;
            }
        }
    }

    async fn cleanup(store: &CloudStore) {
        store
            .cleanup(
                Duration::from_hours(1),
                Duration::from_hours(7 * 24),
                Duration::from_hours(24),
            )
            .await
            .unwrap();
    }

    fn cloud_api(store: &CloudStore) -> Router {
        crate::api(crate::Store::Cloud(store.clone()), small_parts())
    }

    #[test]
    fn cloud_endpoint_requires_https_and_no_embedded_credentials() {
        assert!(validate_endpoint("https://s3.example.com").is_ok());
        assert!(validate_endpoint("http://s3.example.com").is_err());
        assert!(validate_endpoint("https://key:secret@s3.example.com").is_err());
        assert!(validate_endpoint("not a url").is_err());
    }

    #[tokio::test]
    async fn cloud_uploads_resume_complete_once_and_publish_when_done() {
        let (endpoint, bucket, task) = start_mock().await;
        let store = client(&endpoint, "test-bucket");
        let app = cloud_api(&store);
        let token = start(&app, 10, Some("3600")).await;
        for (number, bytes) in [(2, "efgh"), (1, "abcd")] {
            let (status, _, _) = send(&app, part(&token, number, bytes.as_bytes())).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        // A restarted server finds the upload from its token alone.
        let app = cloud_api(&client(&endpoint, "test-bucket"));
        let (_, _, body) = send(&app, crate::tests::status(&token)).await;
        assert_eq!(json(&body)["received"], serde_json::json!([1, 2]));
        let (status, _, _) = send(&app, complete(&token)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        send(&app, part(&token, 3, b"ij")).await;
        let (status, _, body) = send(&app, complete(&token)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json(&body)["expires_in_seconds"], 3600);
        let id = json(&body)["id"].as_str().unwrap().to_owned();
        let uuid = Uuid::parse_str(&id).unwrap();
        {
            let bucket = bucket.lock().unwrap();
            assert!(bucket.uploads.is_empty());
            let marker = &bucket.objects[&marker_key(uuid)];
            assert!(marker.bytes.is_empty());
            let expires: u64 = marker
                .metadata
                .iter()
                .find(|(name, _)| name == EXPIRES_METADATA)
                .unwrap()
                .1
                .parse()
                .unwrap();
            assert!(expires > now_seconds().unwrap() + 3500);
        }
        // Repeating the request after a lost reply returns the same file.
        let (status, _, body) = send(&app, complete(&token)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json(&body)["id"], id.as_str());
        // Cancelling a completed upload leaves the file in place.
        let request = Request::delete(format!("/api/uploads/{token}"))
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&app, request).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (code, headers, body) = send(&app, download(&id)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_LENGTH], "10");
        assert_eq!(body, "abcdefghij");
        let request = Request::get(format!("/api/files/{id}"))
            .header(header::RANGE, "bytes=4-")
            .body(Body::empty())
            .unwrap();
        let (code, headers, body) = send(&app, request).await;
        assert_eq!(code, StatusCode::PARTIAL_CONTENT);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 4-9/10");
        assert_eq!(body, "efghij");
        let (code, _, _) = send(&app, crate::tests::status(&token)).await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        let cancelled = start(&app, 8, None).await;
        send(&app, part(&cancelled, 1, b"abcd")).await;
        let request = Request::delete(format!("/api/uploads/{cancelled}"))
            .body(Body::empty())
            .unwrap();
        send(&app, request).await;
        assert!(bucket.lock().unwrap().uploads.is_empty());
        let (status, _, _) = send(&app, part(&cancelled, 2, b"efgh")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        task.abort();
    }

    #[tokio::test]
    async fn cloud_reads_old_files_and_cleans_up_expired_and_abandoned_ones() {
        let (endpoint, bucket, task) = start_mock().await;
        let store = client(&endpoint, "test-bucket");
        let app = cloud_api(&store);
        let object = |bytes: &'static [u8], expires: &str| Object {
            bytes: Bytes::from_static(bytes),
            metadata: vec![(EXPIRES_METADATA.to_owned(), expires.to_owned())],
            modified: SystemTime::now(),
        };
        let (legacy, old_legacy) = (Uuid::new_v4(), Uuid::new_v4());
        bucket.lock().unwrap().objects.extend([
            (marker_key(legacy), object(b"legacy", "never")),
            (marker_key(old_legacy), object(b"old", "1")),
        ]);
        let live = Uuid::parse_str(&upload(&app, b"live data", Some("0")).await).unwrap();
        let expired = Uuid::parse_str(&upload(&app, b"expired", None).await).unwrap();
        bucket
            .lock()
            .unwrap()
            .objects
            .get_mut(&marker_key(expired))
            .unwrap()
            .metadata = vec![(EXPIRES_METADATA.to_owned(), "1".to_owned())];
        // Completed, but the server stopped before writing the marker.
        let orphan = Uuid::new_v4();
        let upload_id = store.create_upload(orphan, None).await.ok().unwrap();
        store
            .upload_part(orphan, &upload_id, 1, Bytes::from_static(b"lost"))
            .await
            .ok()
            .unwrap();
        let parts = store.list_parts(orphan, &upload_id).await.ok().unwrap();
        store
            .complete_upload(orphan, &upload_id, &parts)
            .await
            .ok()
            .unwrap();
        let abandoned = start(&app, 8, None).await;
        send(&app, part(&abandoned, 1, b"abcd")).await;
        backdate(&bucket, "");

        assert_eq!(read(&store, legacy).await.unwrap(), "legacy");
        assert!(read(&store, old_legacy).await.is_none());
        assert_eq!(read(&store, live).await.unwrap(), "live data");
        assert!(read(&store, expired).await.is_none());
        assert!(read(&store, orphan).await.is_none());
        assert!(store.open(live, 1).await.is_err());
        let file = store.open(live, 100).await.unwrap().unwrap();
        let body = store.read(&file, 2, 5).await.unwrap().unwrap();
        assert_eq!(body.collect().await.unwrap().to_bytes(), "ve d");

        cleanup(&store).await;
        {
            let bucket = bucket.lock().unwrap();
            let mut keys: Vec<&String> = bucket.objects.keys().collect();
            keys.sort();
            let mut expected = [
                marker_key(legacy),
                accessed_key(legacy),
                marker_key(live),
                data_key(live),
                accessed_key(live),
            ];
            expected.sort();
            assert_eq!(keys, expected.iter().collect::<Vec<_>>());
            assert!(bucket.uploads.is_empty());
        }
        assert_eq!(read(&store, live).await.unwrap(), "live data");

        let error_store = client(&endpoint, "error");
        assert!(error_store.open(live, 100).await.is_err());
        assert!(error_store.create_upload(live, None).await.is_err());
        task.abort();
    }

    #[tokio::test]
    async fn cloud_deletes_files_that_go_a_week_without_downloads() {
        let (endpoint, bucket, task) = start_mock().await;
        let store = client(&endpoint, "test-bucket");
        let app = cloud_api(&store);
        let mut ids = Vec::new();
        for data in [&b"kept"[..], b"idle", b"untracked"] {
            ids.push(Uuid::parse_str(&upload(&app, data, Some("0")).await).unwrap());
        }
        let [kept, idle, untracked] = ids[..] else {
            unreachable!()
        };
        for id in [kept, idle] {
            let (status, _, _) = send(&app, download(&id.to_string())).await;
            assert_eq!(status, StatusCode::OK);
        }
        // Every file was uploaded long ago, but only `kept` has been downloaded recently.
        backdate(&bucket, PREFIX);
        bucket
            .lock()
            .unwrap()
            .objects
            .get_mut(&accessed_key(kept))
            .unwrap()
            .modified = SystemTime::now();
        // A file that went unrecorded gets a week from now, which also covers files uploaded
        // before downloads were recorded.
        bucket
            .lock()
            .unwrap()
            .objects
            .remove(&accessed_key(untracked));
        let orphan = Uuid::new_v4();
        store.touch(orphan).await.unwrap();

        cleanup(&store).await;
        let bucket = bucket.lock().unwrap();
        let mut keys: Vec<&String> = bucket.objects.keys().collect();
        keys.sort();
        let mut expected = [kept, untracked]
            .into_iter()
            .flat_map(|id| [marker_key(id), data_key(id), accessed_key(id)])
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(keys, expected.iter().collect::<Vec<_>>());
        assert!(bucket.objects[&accessed_key(untracked)].modified > UNIX_EPOCH);
        task.abort();
    }
}
