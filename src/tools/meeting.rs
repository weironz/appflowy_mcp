//! AI meeting transcription tools. Endpoints mirror AppFlowy-Web's official
//! `meeting-api.ts` (base `/api/meeting/{workspace_id}/v2`).

use std::io::ErrorKind;
use std::path::Path;

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::server::AppServer;
use crate::tools::util::map_tool_error;

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct MeetingUsageRequest {
    pub workspace_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct TranscribeAudioRequest {
    pub workspace_id: String,
    /// Already-downloadable audio URL; skips the file-storage upload.
    /// Mutually exclusive with `path`.
    pub audio_url: Option<String>,
    /// Local audio file; uploaded through a presigned file-storage URL first.
    /// Mutually exclusive with `audio_url`.
    pub path: Option<String>,
    /// Enable per-speaker labelling (diarization). Defaults to false.
    pub speaker_labels: Option<bool>,
    /// ISO language code of the audio (e.g. "en", "zh").
    pub language_code: Option<String>,
    /// Let the service detect the spoken language automatically.
    pub language_detection: Option<bool>,
    /// Audio duration in seconds, echoed verbatim into the
    /// `x-transcription-duration` header. The web client defaults this to 0;
    /// the real duration is not probed client-side here either, so callers
    /// that know it should pass it, otherwise 0 (unknown) is sent.
    pub duration_seconds: Option<i64>,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetTranscriptRequest {
    pub workspace_id: String,
    pub transcript_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct GetParagraphsRequest {
    pub workspace_id: String,
    pub transcript_id: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(default)]
pub struct DeleteTranscriptRequest {
    pub workspace_id: String,
    pub transcript_id: String,
}

/// Meeting API path prefix for a workspace (mirrors `meeting-api.ts`).
fn meeting_base(workspace_id: &str) -> String {
    format!("/api/meeting/{workspace_id}/v2")
}

/// Upload a local audio file to file storage via a presigned URL and return
/// its download URL, which `transcribe-audio` then consumes.
async fn upload_audio(
    server: &AppServer,
    workspace_id: &str,
    path: &str,
) -> Result<String, ErrorData> {
    let file_path = Path::new(path);
    let bytes = tokio::fs::read(file_path).await.map_err(|err| {
        let message = if err.kind() == ErrorKind::NotFound {
            format!("Local audio file not found: {path}")
        } else {
            format!("Cannot read local audio file ({path}): {err}")
        };
        ErrorData::invalid_params(message, None)
    })?;
    let content_type = mime_guess::from_path(file_path)
        .first_raw()
        .map(str::to_string)
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let presigned = server
        .client
        .request(
            Method::POST,
            &format!("/api/file_storage/{workspace_id}/create_presigned_url"),
            None,
            Some(json!({
                "content_length": bytes.len(),
                "file_key_prefix": "audio_file_transcription",
                "content_type": content_type,
            })),
        )
        .await
        .map_err(map_tool_error)?;
    let upload_url = presigned
        .get("upload_url")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ErrorData::internal_error(
                "create_presigned_url response is missing `upload_url`.",
                None,
            )
        })?;
    let download_url = presigned
        .get("download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ErrorData::internal_error(
                "create_presigned_url response is missing `download_url`.",
                None,
            )
        })?;

    // The presigned URL carries its own authorization (per the web client's
    // comment); a standalone client is used so the bearer token is NEVER
    // sent to the storage backend.
    let response = reqwest::Client::new()
        .put(upload_url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(bytes)
        .send()
        .await
        .map_err(|err| {
            ErrorData::internal_error(format!("Presigned audio upload failed: {err}"), None)
        })?;
    let status = response.status();
    if !status.is_success() {
        let snippet: String = response
            .text()
            .await
            .unwrap_or_default()
            .trim()
            .chars()
            .take(200)
            .collect();
        return Err(ErrorData::internal_error(
            format!("Presigned audio upload rejected with HTTP {status}: {snippet}"),
            None,
        ));
    }
    Ok(download_url.to_string())
}

#[tool_router(router = meeting_tool_router, vis = "pub(crate)")]
impl AppServer {
    #[tool(
        name = "appflowy_meeting_usage",
        description = "Get workspace usage and limits (meeting transcription quota is included in this payload)."
    )]
    async fn appflowy_meeting_usage(
        &self,
        Parameters(request): Parameters<MeetingUsageRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!("/api/workspace/{}/usage-and-limit", request.workspace_id),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_meeting_transcribe",
        description = "Start an AI meeting-audio transcription. Provide either `audio_url` (already downloadable) or `path` (a local audio file, uploaded to file storage first). Returns the queued transcript ({id, status, ...}); poll appflowy_meeting_get_transcript for completion."
    )]
    async fn appflowy_meeting_transcribe(
        &self,
        Parameters(request): Parameters<TranscribeAudioRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let audio_url = request.audio_url.filter(|url| !url.is_empty());
        let path = request.path.filter(|path| !path.is_empty());
        if audio_url.is_some() && path.is_some() {
            return Err(ErrorData::invalid_params(
                "audio_url and path are mutually exclusive; provide exactly one audio source.",
                None,
            ));
        }
        let audio_url = match (audio_url, path) {
            (Some(url), None) => url,
            (None, Some(path)) => upload_audio(self, &request.workspace_id, &path).await?,
            (None, None) => {
                return Err(ErrorData::invalid_params(
                    "An audio source is required: either audio_url or path.",
                    None,
                ));
            }
            (Some(_), Some(_)) => unreachable!("mutual exclusion checked above"),
        };
        let speaker_labels = request.speaker_labels.unwrap_or(false);
        // Real duration is not probed client-side (matching the web client);
        // the header defaults to "0" (unknown) unless the caller passes
        // `duration_seconds`.
        let duration = request.duration_seconds.unwrap_or(0).to_string();
        let data = self
            .client
            .request_ext(
                Method::POST,
                &format!("{}/transcribe-audio", meeting_base(&request.workspace_id)),
                None,
                Some(json!({
                    "audio_url": audio_url,
                    "speaker_labels": speaker_labels,
                    "speech_models": [],
                    "language_code": request.language_code,
                    "language_detection": request.language_detection,
                })),
                &[("x-transcription-duration", duration.as_str())],
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_meeting_get_transcript",
        description = "Get a meeting transcript record ({id, status, ...}). Status flows queued -> processing -> completed (or error); poll this tool until status is completed."
    )]
    async fn appflowy_meeting_get_transcript(
        &self,
        Parameters(request): Parameters<GetTranscriptRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!(
                    "{}/get-transcript/{}",
                    meeting_base(&request.workspace_id),
                    request.transcript_id
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_meeting_get_paragraphs",
        description = "Get a meeting transcript's paragraphs with word-level timestamps (available once transcription completes)."
    )]
    async fn appflowy_meeting_get_paragraphs(
        &self,
        Parameters(request): Parameters<GetParagraphsRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        let data = self
            .client
            .request(
                Method::GET,
                &format!(
                    "{}/get-paragraphs/{}",
                    meeting_base(&request.workspace_id),
                    request.transcript_id
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(data))
    }

    #[tool(
        name = "appflowy_meeting_delete_transcript",
        description = "Delete a meeting transcript."
    )]
    async fn appflowy_meeting_delete_transcript(
        &self,
        Parameters(request): Parameters<DeleteTranscriptRequest>,
    ) -> Result<Json<Value>, ErrorData> {
        self.client
            .request(
                Method::DELETE,
                &format!(
                    "{}/delete-transcript/{}",
                    meeting_base(&request.workspace_id),
                    request.transcript_id
                ),
                None,
                None,
            )
            .await
            .map_err(map_tool_error)?;
        Ok(Json(json!({
            "status": "deleted",
            "transcript_id": request.transcript_id,
        })))
    }
}
