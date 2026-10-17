//! Async HTTP client for the AppFlowy Cloud API.
//!
//! Faithful async port of the Python `appflowy_mcp/client.py`. One deliberate
//! consolidation: the Python envelope unwrap (`body.get("data", body)`) was
//! split between the client and ~55 `response_data()` call sites in the MCP
//! server; here [`handle_response`] unwraps a top-level `data` key exactly
//! once on success, so callers receive the payload directly.

use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::{Method, Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

use crate::error::{Error, Result};

pub const DEFAULT_BASE_URL: &str = "https://beta.appflowy.cloud";

/// Max row ids per row/detail request, to keep the `ids` query string well
/// under common URL-length limits when a database has many rows.
const ROW_DETAIL_BATCH_SIZE: usize = 100;

/// AppFlowy-Web `CHUNK_SIZE` from `multipart-upload.types.ts`: 5MB, the S3
/// minimum part size. Also the threshold (their `MULTIPART_THRESHOLD`) at or
/// above which a file moves from single-shot to multipart upload.
pub const MULTIPART_CHUNK_SIZE_BYTES: usize = 5 * 1024 * 1024;

/// AppFlowy-Web `MAX_CONCURRENCY`: max parallel in-flight part uploads.
const MULTIPART_MAX_CONCURRENCY: usize = 3;

/// Per-part retries after the first attempt, with 1s/2s/4s backoff
/// (AppFlowy-Web `MAX_RETRIES`, its exponential-backoff sequence).
const MULTIPART_MAX_PART_RETRIES: u32 = 3;

/// Equivalent of Python's `urllib.parse.quote(s, safe="")`: everything that
/// is not an RFC 3986 unreserved character gets percent-encoded.
const PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: Option<i64>,
}

/// A raw response for binary endpoints (e.g. PDF export): success bodies are
/// delivered untouched; every error shape still surfaces as [`Error`].
pub struct RawResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}

struct ClientInner {
    http: reqwest::Client,
    email: RwLock<Option<String>>,
    password: RwLock<Option<String>>,
    base_url: RwLock<String>,
    access_token: RwLock<Option<String>>,
    refresh_token: RwLock<Option<String>>,
    max_retries: u32,
    backoff_base: f64,
    // Serializes reauth so concurrent 401s don't both spend the rotating
    // refresh token.
    reauth_lock: Mutex<()>,
}

#[derive(Clone)]
pub struct AppFlowyClient {
    inner: Arc<ClientInner>,
}

enum BodyKind<'a> {
    Json(Option<Value>),
    Content { bytes: &'a [u8], content_type: &'a str },
}

impl AppFlowyClient {
    pub fn new(
        email: Option<String>,
        password: Option<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let email = email.filter(|e| !e.is_empty());
        let password = password.filter(|p| !p.is_empty());
        let base_url = base_url.into();
        Self {
            inner: Arc::new(ClientInner {
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
                email: RwLock::new(email),
                password: RwLock::new(password),
                base_url: RwLock::new(base_url.trim_end_matches('/').to_string()),
                access_token: RwLock::new(None),
                refresh_token: RwLock::new(None),
                max_retries: 3,
                backoff_base: 0.5,
                reauth_lock: Mutex::new(()),
            }),
        }
    }

    pub async fn email(&self) -> Option<String> {
        self.inner.email.read().await.clone()
    }

    pub async fn set_email(&self, email: Option<String>) {
        *self.inner.email.write().await = email.filter(|e| !e.is_empty());
    }

    pub async fn password(&self) -> Option<String> {
        self.inner.password.read().await.clone()
    }

    pub async fn set_password(&self, password: Option<String>) {
        *self.inner.password.write().await = password.filter(|p| !p.is_empty());
    }

    pub async fn base_url(&self) -> String {
        self.inner.base_url.read().await.clone()
    }

    pub async fn set_base_url(&self, base_url: impl Into<String>) {
        *self.inner.base_url.write().await = base_url.into().trim_end_matches('/').to_string();
    }

    pub async fn access_token(&self) -> Option<String> {
        self.inner.access_token.read().await.clone()
    }

    pub async fn set_access_token(&self, access_token: Option<String>) {
        *self.inner.access_token.write().await = access_token;
    }

    /// Named `refresh_token_value` because [`AppFlowyClient::refresh_token`]
    /// is the token-refresh API call.
    pub async fn refresh_token_value(&self) -> Option<String> {
        self.inner.refresh_token.read().await.clone()
    }

    pub async fn set_refresh_token(&self, refresh_token: Option<String>) {
        *self.inner.refresh_token.write().await = refresh_token;
    }

    async fn set_token_response(&self, token: &TokenResponse) {
        *self.inner.access_token.write().await = Some(token.access_token.clone());
        *self.inner.refresh_token.write().await = Some(token.refresh_token.clone());
    }

    pub async fn login(&self) -> Result<TokenResponse> {
        let (Some(email), Some(password)) = (self.email().await, self.password().await) else {
            return Err(Error::Validation(
                "Email and password are required for login.".into(),
            ));
        };
        let body = self
            .request_inner(
                Method::POST,
                "/gotrue/token?grant_type=password",
                None,
                Some(json!({ "email": email, "password": password })),
                false,
                &[],
            )
            .await?;
        let token: TokenResponse = serde_json::from_value(body)?;
        self.set_token_response(&token).await;
        Ok(token)
    }

    pub async fn refresh_token(&self) -> Result<TokenResponse> {
        let Some(refresh_token) = self.refresh_token_value().await else {
            return Err(Error::Validation(
                "No refresh token available. Please login first.".into(),
            ));
        };
        let body = self
            .request_inner(
                Method::POST,
                "/gotrue/token?grant_type=refresh_token",
                None,
                Some(json!({ "refresh_token": refresh_token })),
                false,
                &[],
            )
            .await?;
        let token: TokenResponse = serde_json::from_value(body)?;
        self.set_token_response(&token).await;
        Ok(token)
    }

    pub async fn get_database_fields(&self, workspace_id: &str, database_id: &str) -> Result<Value> {
        self.request(
            Method::GET,
            &format!("/api/workspace/{workspace_id}/database/{database_id}/fields"),
            None,
            None,
        )
        .await
    }

    pub async fn get_database_row_ids(&self, workspace_id: &str, database_id: &str) -> Result<Value> {
        self.request(
            Method::GET,
            &format!("/api/workspace/{workspace_id}/database/{database_id}/row"),
            None,
            None,
        )
        .await
    }

    pub async fn get_database_row_ids_updated(
        &self,
        workspace_id: &str,
        database_id: &str,
        after: Option<&str>,
    ) -> Result<Value> {
        let params: Vec<(&str, String)> = after
            .map(|after| vec![("after", after.to_string())])
            .unwrap_or_default();
        self.request(
            Method::GET,
            &format!("/api/workspace/{workspace_id}/database/{database_id}/row/updated"),
            Some(&params),
            None,
        )
        .await
    }

    /// Returns a JSON array with the merged details of all batches.
    pub async fn get_database_row_details(
        &self,
        workspace_id: &str,
        database_id: &str,
        row_ids: &[String],
        with_doc: Option<bool>,
    ) -> Result<Value> {
        if row_ids.is_empty() {
            return Err(Error::Validation("At least one row ID is required.".into()));
        }
        let path = format!("/api/workspace/{workspace_id}/database/{database_id}/row/detail");
        let mut results = Vec::new();
        for chunk in row_ids.chunks(ROW_DETAIL_BATCH_SIZE) {
            let mut params = vec![("ids", chunk.join(","))];
            if let Some(with_doc) = with_doc {
                params.push(("with_doc", with_doc.to_string()));
            }
            let body = self.request(Method::GET, &path, Some(&params), None).await?;
            match body {
                Value::Array(items) => results.extend(items),
                other => results.push(other),
            }
        }
        Ok(Value::Array(results))
    }

    /// The Python client stringified the response (`str(body.get("data", ""))`);
    /// here the unwrapped payload is returned as-is and stringification is
    /// left to the caller.
    pub async fn create_database_row(
        &self,
        workspace_id: &str,
        database_id: &str,
        cells: Option<Value>,
        document: Option<String>,
    ) -> Result<Value> {
        self.request(
            Method::POST,
            &format!("/api/workspace/{workspace_id}/database/{database_id}/row"),
            None,
            Some(json!({
                "cells": cells.unwrap_or_else(|| json!({})),
                "document": document,
            })),
        )
        .await
    }

    /// See [`AppFlowyClient::create_database_row`] for the return-value note.
    pub async fn upsert_database_row(
        &self,
        workspace_id: &str,
        database_id: &str,
        pre_hash: &str,
        cells: Option<Value>,
        document: Option<String>,
    ) -> Result<Value> {
        self.request(
            Method::PUT,
            &format!("/api/workspace/{workspace_id}/database/{database_id}/row"),
            None,
            Some(json!({
                "pre_hash": pre_hash,
                "cells": cells.unwrap_or_else(|| json!({})),
                "document": document,
            })),
        )
        .await
    }

    /// On success returns the upload response object augmented with
    /// `workspace_id` / `parent_dir` / `url` / `content_type` /
    /// `content_length`, mirroring the Python client.
    pub async fn upload_file_blob_v1(
        &self,
        workspace_id: &str,
        parent_dir: &str,
        content: &[u8],
        content_type: &str,
    ) -> Result<Value> {
        let encoded_parent_dir = utf8_percent_encode(parent_dir, PATH_ENCODE_SET).to_string();
        // `body` is already envelope-unwrapped by `handle_response`, which is
        // exactly the `body.get("data", body)` fallback the Python client
        // applied here.
        let data = self
            .request_content_inner(
                Method::PUT,
                &format!("/api/file_storage/{workspace_id}/v1/blob/{encoded_parent_dir}"),
                content,
                content_type,
                true,
            )
            .await?;
        let mut data = match data {
            Value::Object(map) => map,
            _ => {
                return Err(Error::Other(
                    "Expected file upload response data to be an object.".into(),
                ));
            }
        };
        let file_id = data.get("file_id").cloned().unwrap_or(Value::Null);
        let file_id_str = match &file_id {
            Value::String(s) if !s.is_empty() => s.clone(),
            other if !other.is_null() => other.to_string(),
            _ => {
                return Err(Error::Other(
                    "File upload response did not include file_id.".into(),
                ));
            }
        };
        let url = self
            .file_blob_url_v1(workspace_id, parent_dir, &file_id_str)
            .await;
        data.insert("workspace_id".into(), json!(workspace_id));
        data.insert("parent_dir".into(), json!(parent_dir));
        data.insert("url".into(), json!(url));
        data.insert("content_type".into(), json!(content_type));
        data.insert("content_length".into(), json!(content.len()));
        Ok(Value::Object(data))
    }

    pub async fn file_blob_url_v1(
        &self,
        workspace_id: &str,
        parent_dir: &str,
        file_id: &str,
    ) -> String {
        let encoded_parent_dir = utf8_percent_encode(parent_dir, PATH_ENCODE_SET).to_string();
        let encoded_file_id = utf8_percent_encode(file_id, PATH_ENCODE_SET).to_string();
        format!(
            "{}/api/file_storage/{workspace_id}/v1/blob/{encoded_parent_dir}/{encoded_file_id}",
            self.base_url().await
        )
    }

    /// Multipart upload, following AppFlowy-Web `multipart-upload.ts`:
    /// create session → PUT each [`MULTIPART_CHUNK_SIZE_BYTES`] chunk as a
    /// numbered part (1-based) with bounded parallelism and per-part retries
    /// → complete; any failure aborts the session best-effort before the
    /// original error is returned. On success returns the same shape as
    /// [`Self::upload_file_blob_v1`].
    pub async fn upload_file_multipart(
        &self,
        workspace_id: &str,
        parent_dir: &str,
        content: &[u8],
        content_type: &str,
    ) -> Result<Value> {
        // The client picks the file id up front (uuid v4) so create/complete
        // and the final blob URL can all reference it.
        let file_id = uuid::Uuid::new_v4().to_string();
        let data = self
            .request(
                Method::POST,
                &format!("/api/file_storage/{workspace_id}/create_upload"),
                None,
                Some(json!({
                    "file_id": file_id,
                    "parent_dir": parent_dir,
                    "content_type": content_type,
                    "file_size": content.len(),
                })),
            )
            .await?;
        let upload_id = data
            .get("upload_id")
            .and_then(Value::as_str)
            .filter(|upload_id| !upload_id.is_empty())
            .map(str::to_string);
        let Some(upload_id) = upload_id else {
            return Err(Error::Other(
                "Create multipart upload response did not include upload_id.".into(),
            ));
        };

        match self
            .upload_parts_and_complete(
                workspace_id,
                parent_dir,
                &file_id,
                &upload_id,
                content,
                content_type,
            )
            .await
        {
            Ok(data) => Ok(data),
            Err(err) => {
                // Best-effort abort so the server does not leak the partial
                // session; its result never masks the original error.
                let encoded_parent_dir =
                    utf8_percent_encode(parent_dir, PATH_ENCODE_SET).to_string();
                let _ = self
                    .request(
                        Method::DELETE,
                        &format!(
                            "/api/file_storage/{workspace_id}/upload/{encoded_parent_dir}/{file_id}/{upload_id}"
                        ),
                        None,
                        None,
                    )
                    .await;
                Err(err)
            }
        }
    }

    /// Parts phase (parallel, retried) plus the complete call; builds the
    /// [`Self::upload_file_blob_v1`]-compatible result object on success.
    async fn upload_parts_and_complete(
        &self,
        workspace_id: &str,
        parent_dir: &str,
        file_id: &str,
        upload_id: &str,
        content: &[u8],
        content_type: &str,
    ) -> Result<Value> {
        use futures::StreamExt as _;

        let encoded_parent_dir = utf8_percent_encode(parent_dir, PATH_ENCODE_SET).to_string();
        let chunks: Vec<(usize, &[u8])> = content
            .chunks(MULTIPART_CHUNK_SIZE_BYTES)
            .enumerate()
            .map(|(index, bytes)| (index + 1, bytes))
            .collect();
        // Futures are built in a plain loop (not a `.map` closure) so nothing
        // references a per-item borrow; each future owns its path and the
        // `&[u8]` slice into `content`.
        let mut uploads = Vec::with_capacity(chunks.len());
        for (part_number, bytes) in chunks {
            let path = format!(
                "/api/file_storage/{workspace_id}/upload_part/{encoded_parent_dir}/{file_id}/{upload_id}/{part_number}"
            );
            uploads.push(async move {
                let mut attempt = 0u32;
                loop {
                    match self
                        .request_content_inner(
                            Method::PUT,
                            &path,
                            bytes,
                            "application/octet-stream",
                            true,
                        )
                        .await
                    {
                        Ok(data) => {
                            let e_tag = data
                                .get("e_tag")
                                .and_then(Value::as_str)
                                .filter(|e_tag| !e_tag.is_empty())
                                .map(str::to_string);
                            let Some(e_tag) = e_tag else {
                                return Err(Error::Other(format!(
                                    "Upload part {part_number} response did not include e_tag."
                                )));
                            };
                            return Ok::<(usize, String), Error>((part_number, e_tag));
                        }
                        Err(err)
                            if attempt < MULTIPART_MAX_PART_RETRIES
                                && Self::is_transient_part_error(&err) =>
                        {
                            attempt += 1;
                            tokio::time::sleep(Duration::from_secs(1u64 << (attempt - 1))).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
            });
        }
        let results: Vec<Result<(usize, String)>> = futures::stream::iter(uploads)
            .buffer_unordered(MULTIPART_MAX_CONCURRENCY)
            .collect()
            .await;
        let mut parts = Vec::with_capacity(results.len());
        for result in results {
            parts.push(result?);
        }
        // Completion requires the parts ordered by part_number even though
        // they finished out of order.
        parts.sort_by_key(|(part_number, _)| *part_number);
        let parts: Vec<Value> = parts
            .into_iter()
            .map(|(part_number, e_tag)| json!({ "part_number": part_number, "e_tag": e_tag }))
            .collect();
        let data = self
            .request(
                Method::PUT,
                &format!("/api/file_storage/{workspace_id}/complete_upload"),
                None,
                Some(json!({
                    "file_id": file_id,
                    "parent_dir": parent_dir,
                    "upload_id": upload_id,
                    "parts": parts,
                })),
            )
            .await?;
        let mut data = match data {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let url = self
            .file_blob_url_v1(workspace_id, parent_dir, file_id)
            .await;
        data.insert("file_id".into(), json!(file_id));
        data.insert("workspace_id".into(), json!(workspace_id));
        data.insert("parent_dir".into(), json!(parent_dir));
        data.insert("url".into(), json!(url));
        data.insert("content_type".into(), json!(content_type));
        data.insert("content_length".into(), json!(content.len()));
        Ok(Value::Object(data))
    }

    /// Per-part retry policy: transport failures, 429, and 5xx are transient;
    /// any other 4xx (including workspace-limit errors surfaced as 402/business
    /// codes) fails immediately.
    fn is_transient_part_error(err: &Error) -> bool {
        matches!(err, Error::Transport(_))
            || matches!(err.status(), Some(status) if status == 429 || status >= 500)
    }

    /// Generic JSON request used by the MCP tools / importer / CLI.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        params: Option<&[(&str, String)]>,
        json_body: Option<Value>,
    ) -> Result<Value> {
        self.request_inner(method, path, params, json_body, true, &[])
            .await
    }

    /// Like [`Self::request`], with extra per-call headers (e.g. `X-Host` for
    /// the import/export tasks, `device-id` for comment mutations).
    pub async fn request_ext(
        &self,
        method: Method,
        path: &str,
        params: Option<&[(&str, String)]>,
        json_body: Option<Value>,
        extra_headers: &[(&str, &str)],
    ) -> Result<Value> {
        self.request_inner(method, path, params, json_body, true, extra_headers)
            .await
    }

    /// Raw response for binary endpoints (e.g. PDF export). Auth/retry go
    /// through the same `send` path; JSON responses — including every API
    /// error shape — are still mapped onto [`Error`]; success bodies are
    /// returned untouched regardless of type.
    pub async fn request_bytes_ext(
        &self,
        method: Method,
        path: &str,
        params: Option<&[(&str, String)]>,
        json_body: Option<Value>,
        extra_headers: &[(&str, &str)],
    ) -> Result<RawResponse> {
        let json_payload = json_body.map(|body| match body {
            Value::Object(map) => {
                Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
            }
            other => other,
        });
        let response = self
            .send(&method, path, params, BodyKind::Json(json_payload), true, extra_headers)
            .await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response.bytes().await?.to_vec();

        // Error mapping mirrors `handle_response`; success bodies (PDF bytes)
        // fall through untouched regardless of content type.
        let looks_json = content_type
            .as_deref()
            .is_some_and(|ct| ct.contains("json"))
            || body.first() == Some(&b'{');
        if looks_json {
            if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&body) {
                let code = map.get("code").and_then(Value::as_i64);
                if status >= 400 || code.is_some_and(|code| code != 0) {
                    let message = map
                        .get("message")
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .map_or_else(|| format!("HTTP {status}"), str::to_string);
                    return Err(Error::api(message, Some(status), code));
                }
            }
        }
        if status >= 400 {
            let snippet: String = String::from_utf8_lossy(&body)
                .trim()
                .chars()
                .take(200)
                .collect();
            return Err(Error::NonJsonBody {
                status,
                body_snippet: snippet,
            });
        }

        Ok(RawResponse {
            status,
            content_type,
            headers,
            body,
        })
    }

    /// Generic raw-body request (file uploads) with JSON response handling.
    pub async fn request_content(
        &self,
        method: Method,
        path: &str,
        content: &[u8],
        content_type: &str,
    ) -> Result<Value> {
        self.request_content_inner(method, path, content, content_type, true)
            .await
    }

    async fn request_inner(
        &self,
        method: Method,
        path: &str,
        params: Option<&[(&str, String)]>,
        json_body: Option<Value>,
        allow_reauth: bool,
        extra_headers: &[(&str, &str)],
    ) -> Result<Value> {
        // Some endpoints (remove members, invite) take a top-level JSON array;
        // only object bodies get the drop-null-values treatment.
        let json_payload = json_body.map(|body| match body {
            Value::Object(map) => {
                Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).collect())
            }
            other => other,
        });
        let response = self
            .send(
                &method,
                path,
                params,
                BodyKind::Json(json_payload),
                allow_reauth,
                extra_headers,
            )
            .await?;
        Self::handle_response(response).await
    }

    async fn request_content_inner(
        &self,
        method: Method,
        path: &str,
        content: &[u8],
        content_type: &str,
        allow_reauth: bool,
    ) -> Result<Value> {
        let response = self
            .send(
                &method,
                path,
                None,
                BodyKind::Content {
                    bytes: content,
                    content_type,
                },
                allow_reauth,
                &[],
            )
            .await?;
        Self::handle_response(response).await
    }

    /// Join `base_url` + `path`, appending any query params. Built manually
    /// because this crate does not enable reqwest's `query` feature
    /// (`.query()`); note a `path` may itself already carry a query string
    /// (e.g. `...?grant_type=password`).
    async fn build_url(&self, path: &str, params: Option<&[(&str, String)]>) -> String {
        let mut url = format!("{}{path}", self.base_url().await);
        if let Some(params) = params {
            let separator = if url.contains('?') { '&' } else { '?' };
            let encoded: Vec<String> = params
                .iter()
                .map(|(name, value)| {
                    format!(
                        "{name}={}",
                        utf8_percent_encode(value, PATH_ENCODE_SET)
                    )
                })
                .collect();
            if !encoded.is_empty() {
                url.push(separator);
                url.push_str(&encoded.join("&"));
            }
        }
        url
    }

    /// Send a request with one auto-reauth on 401 and backoff on 429/5xx.
    ///
    /// The request is rebuilt on every attempt, reading a fresh Authorization
    /// header, so a token refreshed mid-flight is picked up. A 429 is retried
    /// for any method (the request was rejected, not applied); a 5xx is only
    /// retried for idempotent methods to avoid duplicating a write the server
    /// may have applied before erroring.
    async fn send(
        &self,
        method: &Method,
        path: &str,
        params: Option<&[(&str, String)]>,
        body: BodyKind<'_>,
        allow_reauth: bool,
        extra_headers: &[(&str, &str)],
    ) -> Result<Response> {
        let idempotent = matches!(
            *method,
            Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS
        );
        let mut attempt = 0u32;
        let mut reauthed = false;
        loop {
            let token_before = self.access_token().await;
            let mut request = self
                .inner
                .http
                .request(method.clone(), self.build_url(path, params).await)
                .header("Accept", "application/json");
            if let Some(token) = &token_before {
                request = request.bearer_auth(token);
            }
            request = match &body {
                BodyKind::Json(payload) => match payload {
                    Some(payload) => request.json(payload),
                    None => request.header("Content-Type", "application/json"),
                },
                BodyKind::Content { bytes, content_type } => {
                    request.header("Content-Type", *content_type).body(bytes.to_vec())
                }
            };
            for (name, value) in extra_headers {
                request = request.header(*name, *value);
            }
            let response = request.send().await?;
            let status = response.status();

            if status == StatusCode::UNAUTHORIZED && allow_reauth && !reauthed {
                reauthed = true;
                if self.reauthenticate(token_before).await {
                    continue;
                }
            }

            let retryable = status == StatusCode::TOO_MANY_REQUESTS
                || (status.is_server_error() && idempotent);
            if retryable && attempt < self.inner.max_retries {
                let delay = self.retry_delay(&response, attempt);
                attempt += 1;
                tokio::time::sleep(delay).await;
                continue;
            }

            return Ok(response);
        }
    }

    /// Refresh the access token, falling back to a fresh login.
    ///
    /// Serialized so concurrent 401s don't both spend the rotating refresh
    /// token; a task that finds the token already changed just reuses it.
    async fn reauthenticate(&self, token_before: Option<String>) -> bool {
        let _guard = self.inner.reauth_lock.lock().await;
        if token_before.is_some() && self.access_token().await != token_before {
            return true; // another task already refreshed
        }
        // The refresh/login calls recurse back through `send`; `Box::pin`
        // breaks the otherwise infinitely sized future cycle.
        if self.refresh_token_value().await.is_some()
            && Box::pin(self.refresh_token()).await.is_ok()
        {
            return true;
        }
        if self.email().await.is_some()
            && self.password().await.is_some()
            && Box::pin(self.login()).await.is_ok()
        {
            return true;
        }
        false
    }

    /// Delay before the next retry: the `Retry-After` header as fractional
    /// seconds when parseable, falling back to `0.5 * 2^attempt`.
    fn retry_delay(&self, response: &Response, attempt: u32) -> Duration {
        let secs = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|secs| *secs >= 0.0)
            .unwrap_or_else(|| self.inner.backoff_base * 2f64.powi(attempt as i32));
        Duration::from_secs_f64(secs)
    }

    /// Parse the response body, map every failure shape onto [`Error`], and
    /// unwrap a top-level `data` key on success (`body.get("data", body)`).
    async fn handle_response(response: Response) -> Result<Value> {
        let status = response.status().as_u16();
        let text = response.text().await?;
        let body: Value = match serde_json::from_str(&text) {
            Ok(body) => body,
            Err(_) => {
                let snippet: String = text.trim().chars().take(200).collect();
                return Err(Error::NonJsonBody {
                    status,
                    body_snippet: snippet,
                });
            }
        };

        if status >= 400 {
            if let Value::Object(map) = &body {
                let message = map
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .map_or_else(|| format!("HTTP {status}"), str::to_string);
                let code = map.get("code").and_then(Value::as_i64);
                return Err(Error::api(message, Some(status), code));
            }
            let message = if text.is_empty() {
                format!("HTTP {status}")
            } else {
                text
            };
            return Err(Error::api(message, Some(status), None));
        }

        // AppFlowy returns business errors as HTTP 200 with a non-zero code in
        // the {code, message, data} envelope (e.g. 1026 workspace limit).
        if let Value::Object(map) = &body {
            if let Some(code) = map.get("code").and_then(Value::as_i64) {
                if code != 0 {
                    let message = map
                        .get("message")
                        .and_then(Value::as_str)
                        .filter(|message| !message.is_empty())
                        .unwrap_or("AppFlowy error")
                        .to_string();
                    return Err(Error::api(message, Some(status), Some(code)));
                }
            }
        }

        match body {
            Value::Object(mut map) => match map.remove("data") {
                Some(data) => Ok(data),
                None => Ok(Value::Object(map)),
            },
            other => Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of `test_reauth_skips_when_token_already_refreshed`: the token seen
    /// at request time was "t" but it is now "already-refreshed", so another
    /// task refreshed it and this call must not refresh again. `base_url`
    /// points at an unroutable address: if the early return were missing, the
    /// refresh/login fallback would fail on transport and return false.
    #[tokio::test]
    async fn reauth_skips_when_token_already_refreshed() {
        let client = AppFlowyClient::new(
            Some("e".into()),
            Some("p".into()),
            "http://127.0.0.1:1",
        );
        client.set_refresh_token(Some("r".into())).await;
        client
            .set_access_token(Some("already-refreshed".into()))
            .await;
        assert!(client.reauthenticate(Some("t".into())).await);
    }
}
