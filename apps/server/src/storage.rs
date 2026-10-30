use std::{
    env,
    error::Error,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region, RequestChecksumCalculation},
    primitives::ByteStream,
};
use axum::body::Bytes;
use axum::http::Uri;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

const PREFIX: &str = "hokan/";
const EXPIRES_METADATA: &str = "expires-at";
type StorageError = Box<dyn Error + Send + Sync>;

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
            Ok(expires <= SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
        }
        None => Err("Stored object has missing expiry metadata".into()),
    }
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

    pub async fn put(
        &self,
        id: Uuid,
        bytes: Bytes,
        lifetime: Option<Duration>,
    ) -> Result<(), StorageError> {
        let expires = match lifetime {
            Some(lifetime) => (SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
                + lifetime.as_secs())
            .to_string(),
            None => "never".to_owned(),
        };
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(format!("{PREFIX}{id}"))
            .content_type("application/octet-stream")
            .metadata(EXPIRES_METADATA, expires)
            .body(ByteStream::from(bytes))
            .send()
            .await?;
        Ok(())
    }

    pub async fn get(&self, id: Uuid, max_bytes: usize) -> Result<Option<Bytes>, StorageError> {
        let key = format!("{PREFIX}{id}");
        let response = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error)
                if error
                    .raw_response()
                    .is_some_and(|response| response.status().as_u16() == 404) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(Box::new(error)),
        };
        if is_expired(
            response
                .metadata()
                .and_then(|metadata| metadata.get(EXPIRES_METADATA)),
        )? {
            return Ok(None);
        }
        if response.content_length().is_none_or(|length| {
            usize::try_from(length)
                .ok()
                .is_none_or(|length| length > max_bytes)
        }) {
            return Err(
                "Stored object exceeds the maximum allowed size or has no content length".into(),
            );
        }
        // Do not trust Content-Length alone: a misbehaving endpoint may stream more bytes.
        let mut reader = response.body.into_async_read().take(max_bytes as u64 + 1);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        if bytes.len() > max_bytes {
            return Err("Stored object exceeds the maximum allowed size".into());
        }
        Ok(Some(Bytes::from(bytes)))
    }

    // Only touch objects in Hokan's namespace with a UUID name. S3 ListObjects returns
    // the last modification time, so cleanup remains possible after a server restart.
    pub async fn cleanup(&self, lifetime: Duration) -> Result<(), StorageError> {
        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .saturating_sub(lifetime.as_secs());
        let mut token = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(PREFIX)
                .set_continuation_token(token)
                .send()
                .await?;
            for object in page.contents() {
                let Some(key) = object.key() else { continue };
                if key
                    .strip_prefix(PREFIX)
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .is_none()
                {
                    continue;
                }
                if object
                    .last_modified()
                    .is_some_and(|time| u64::try_from(time.secs()).is_ok_and(|secs| secs <= cutoff))
                {
                    let head = self
                        .client
                        .head_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .send()
                        .await?;
                    if !is_expired(
                        head.metadata()
                            .and_then(|metadata| metadata.get(EXPIRES_METADATA)),
                    )? {
                        continue;
                    }
                    self.client
                        .delete_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .send()
                        .await?;
                }
            }
            if page.is_truncated() != Some(true) {
                break;
            }
            token = Some(
                page.next_continuation_token()
                    .ok_or("Missing S3 continuation token")?
                    .to_owned(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::State,
        http::{Request, StatusCode, header},
        response::Response,
    };
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct Object {
        bytes: Bytes,
        expires: String,
    }
    type Objects = Arc<Mutex<BTreeMap<String, Object>>>;

    async fn mock_s3(State(objects): State<Objects>, request: Request<Body>) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let query = request.uri().query().unwrap_or("").to_owned();
        if path.starts_with("/error/") {
            return Response::builder().status(500).body(Body::empty()).unwrap();
        }
        if path == "/test-bucket/" && method == "GET" {
            let after = query
                .split('&')
                .find_map(|part| part.strip_prefix("continuation-token="));
            let objects = objects.lock().unwrap();
            let mut entries = objects.keys().filter(|key| {
                after.is_none_or(|after| key.strip_prefix(PREFIX).unwrap_or(key) > after)
            });
            let entry = entries.next().cloned();
            let more = entries.next().is_some();
            let contents = entry.as_ref().map(|key| format!("<Contents><Key>{key}</Key><LastModified>2000-01-01T00:00:00.000Z</LastModified><Size>1</Size></Contents>")).unwrap_or_default();
            let token = if more {
                format!(
                    "<NextContinuationToken>{}</NextContinuationToken>",
                    entry.unwrap().strip_prefix(PREFIX).unwrap_or("")
                )
            } else {
                String::new()
            };
            let xml = format!(
                "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsTruncated>{more}</IsTruncated>{contents}{token}</ListBucketResult>"
            );
            return Response::builder()
                .header(header::CONTENT_TYPE, "application/xml")
                .body(Body::from(xml))
                .unwrap();
        }
        let Some(key) = path.strip_prefix("/test-bucket/") else {
            return Response::builder().status(404).body(Body::empty()).unwrap();
        };
        let key = key.to_owned();
        match method.as_str() {
            "PUT" => {
                let expires = request
                    .headers()
                    .get("x-amz-meta-expires-at")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                let bytes = to_bytes(request.into_body(), 1024).await.unwrap();
                objects
                    .lock()
                    .unwrap()
                    .insert(key, Object { bytes, expires });
                Response::builder().status(200).body(Body::empty()).unwrap()
            }
            "GET" | "HEAD" => {
                let object = objects.lock().unwrap().get(&key).cloned();
                match object {
                    Some(object) => Response::builder()
                        .header("x-amz-meta-expires-at", object.expires)
                        .header(header::CONTENT_LENGTH, object.bytes.len())
                        .body(if method == "HEAD" {
                            Body::empty()
                        } else {
                            Body::from(object.bytes)
                        })
                        .unwrap(),
                    None => Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::from("<Error><Code>NoSuchKey</Code></Error>"))
                        .unwrap(),
                }
            }
            "DELETE" => {
                objects.lock().unwrap().remove(&key);
                Response::builder().status(204).body(Body::empty()).unwrap()
            }
            _ => Response::builder().status(405).body(Body::empty()).unwrap(),
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

    #[test]
    fn cloud_endpoint_requires_https_and_no_embedded_credentials() {
        assert!(validate_endpoint("https://s3.example.com").is_ok());
        assert!(validate_endpoint("http://s3.example.com").is_err());
        assert!(validate_endpoint("https://key:secret@s3.example.com").is_err());
        assert!(validate_endpoint("not a url").is_err());
    }

    #[tokio::test]
    async fn cloud_round_trip_restart_expiry_cleanup_and_errors() {
        let objects: Objects = Arc::new(Mutex::new(BTreeMap::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(mock_s3).with_state(objects.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = client(&endpoint, "test-bucket");
        let live = Uuid::new_v4();
        let expired1 = Uuid::new_v4();
        let expired2 = Uuid::new_v4();
        store
            .put(
                live,
                Bytes::from_static(b"ciphertext"),
                Some(Duration::from_hours(24)),
            )
            .await
            .unwrap();
        store
            .put(expired1, Bytes::from_static(b"old"), Some(Duration::ZERO))
            .await
            .unwrap();
        store
            .put(expired2, Bytes::from_static(b"old"), Some(Duration::ZERO))
            .await
            .unwrap();
        let permanent = Uuid::new_v4();
        store
            .put(permanent, Bytes::from_static(b"permanent"), None)
            .await
            .unwrap();
        let restarted = client(&endpoint, "test-bucket");
        assert_eq!(
            restarted.get(live, 100).await.unwrap().unwrap(),
            "ciphertext"
        );
        assert!(restarted.get(expired1, 100).await.unwrap().is_none());
        assert!(restarted.get(Uuid::new_v4(), 100).await.unwrap().is_none());
        assert!(restarted.get(live, 1).await.is_err());
        restarted.cleanup(Duration::from_hours(24)).await.unwrap();
        assert!(restarted.get(live, 100).await.unwrap().is_some());
        assert_eq!(
            restarted.get(permanent, 100).await.unwrap().unwrap(),
            "permanent"
        );
        assert!(
            !objects
                .lock()
                .unwrap()
                .contains_key(&format!("{PREFIX}{expired1}"))
        );
        assert!(
            !objects
                .lock()
                .unwrap()
                .contains_key(&format!("{PREFIX}{expired2}"))
        );
        let error_store = client(&endpoint, "error");
        assert!(error_store.get(live, 100).await.is_err());
        assert!(
            error_store
                .put(
                    live,
                    Bytes::from_static(b"data"),
                    Some(Duration::from_secs(1))
                )
                .await
                .is_err()
        );
        task.abort();
    }
}
