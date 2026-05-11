//! nightdrive-youtube — YouTube Data API v3 client.
//!
//! Scope is intentionally tight: we only need
//!   * `videos.insert` (resumable upload)
//!   * `thumbnails.set`
//!   * `videos.update` (for schedule + altered-content disclosure)
//!
//! Hand-rolled to avoid pulling in the 80+ deps of `google-youtube3`.
//!
//! OAuth2 refresh-token flow:
//!   1. One-time: run `nightdrive-cli youtube auth` to get a refresh token.
//!   2. Each upload: exchange refresh_token -> short-lived access_token.
//!   3. Use access_token as Bearer on the upload requests.

use async_trait::async_trait;
use nightdrive_core::{CompositionSpec, NightdriveError, NightdriveResult, TrackPaths};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tracing::{debug, info, instrument, warn};

mod bootstrap;
pub use bootstrap::bootstrap_refresh_token;

const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const VIDEOS_INSERT_URL: &str = "https://www.googleapis.com/upload/youtube/v3/videos";
const THUMBNAILS_SET_URL: &str = "https://www.googleapis.com/upload/youtube/v3/thumbnails/set";
const VIDEOS_UPDATE_URL: &str = "https://www.googleapis.com/youtube/v3/videos";

/// 8 MB chunks. YouTube requires chunks be a multiple of 256 KB except for the
/// last; 8 MB is the conventional default and gives good throughput on typical
/// home connections without burning much RAM per request.
pub const DEFAULT_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// Retry budget per chunk: 1 attempt + 2 retries with exponential backoff.
const PER_CHUNK_MAX_ATTEMPTS: u32 = 3;

// =============================================================================
// Public API
// =============================================================================

#[derive(Debug, Clone)]
pub struct YoutubeCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
}

impl YoutubeCredentials {
    pub fn from_env() -> NightdriveResult<Self> {
        Ok(Self {
            client_id: env("NIGHTDRIVE_YT_CLIENT_ID")?,
            client_secret: env("NIGHTDRIVE_YT_CLIENT_SECRET")?,
            refresh_token: env("NIGHTDRIVE_YT_REFRESH_TOKEN")?,
        })
    }
}

fn env(key: &str) -> NightdriveResult<String> {
    std::env::var(key).map_err(|_| NightdriveError::Youtube(format!("missing env: {key}")))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Privacy {
    Private,
    Unlisted,
    Public,
}

#[derive(Debug, Clone)]
pub struct UploadRequest<'a> {
    pub spec: &'a CompositionSpec,
    pub paths: &'a TrackPaths,
    pub privacy: Privacy,
    pub scheduled_publish_at: Option<chrono::DateTime<chrono::Utc>>,
    pub declare_synthetic_content: bool,
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub video_id: String,
    pub upload_url: String,
}

/// What [`YoutubeClient::update_video`] is allowed to change. Anything left
/// `None` is left untouched on the server side.
#[derive(Debug, Default, Clone)]
pub struct VideoUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub category_id: Option<String>,
    pub privacy: Option<Privacy>,
    pub scheduled_publish_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[async_trait]
pub trait YoutubeUploader: Send + Sync {
    async fn upload_video(&self, req: UploadRequest<'_>) -> NightdriveResult<UploadResult>;
    async fn set_thumbnail(&self, video_id: &str, thumb_path: &Path) -> NightdriveResult<()>;
}

/// Result of a single chunk PUT.
#[derive(Debug)]
enum ChunkOutcome {
    /// Final 200/201 received — the upload is complete. Server returned the
    /// video resource with its newly-minted id.
    Complete(VideoResource),
    /// 308 Resume Incomplete — server received bytes up to (and including) this
    /// offset. Next chunk should start at `next_byte`.
    ResumeAt { next_byte: u64 },
}

#[derive(Debug, Deserialize)]
struct VideoResource {
    id: String,
}

// =============================================================================
// Client
// =============================================================================

pub struct YoutubeClient {
    http: reqwest::Client,
    creds: YoutubeCredentials,
}

impl YoutubeClient {
    pub fn new(creds: YoutubeCredentials) -> NightdriveResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(900))    // long upload-friendly
            .build()
            .map_err(|e| NightdriveError::Youtube(format!("client build: {e}")))?;
        Ok(Self { http, creds })
    }

    /// Exchange refresh token for a fresh access token.
    #[instrument(skip(self))]
    async fn access_token(&self) -> NightdriveResult<String> {
        let params = [
            ("client_id", self.creds.client_id.as_str()),
            ("client_secret", self.creds.client_secret.as_str()),
            ("refresh_token", self.creds.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];
        let resp = self
            .http
            .post(OAUTH_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("token request: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Youtube(format!(
                "oauth refresh failed {status}: {text}"
            )));
        }
        #[derive(Deserialize)]
        struct TokenResp {
            access_token: String,
            expires_in: u64,
        }
        let t: TokenResp = resp
            .json()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("decode token: {e}")))?;
        debug!(expires_in = t.expires_in, "got access token");
        Ok(t.access_token)
    }
}

#[async_trait]
impl YoutubeUploader for YoutubeClient {
    #[instrument(
        skip(self, req),
        fields(
            track_id = %req.spec.track_id,
            title = %req.spec.youtube.title,
            privacy = ?req.privacy,
        )
    )]
    async fn upload_video(&self, req: UploadRequest<'_>) -> NightdriveResult<UploadResult> {
        let access = self.access_token().await?;
        let video_path = req.paths.final_mp4();
        let file_size = tokio::fs::metadata(&video_path)
            .await
            .map_err(|e| NightdriveError::Io {
                path: video_path.display().to_string(),
                source: e,
            })?
            .len();

        // Build the metadata payload.
        let mut snippet = serde_json::json!({
            "title": req.spec.youtube.title,
            "description": req.spec.youtube.description,
            "tags": req.spec.youtube.tags,
            "categoryId": req.spec.youtube.category_id,
        });
        let mut status = serde_json::json!({
            "privacyStatus": match req.privacy {
                Privacy::Private => "private",
                Privacy::Unlisted => "unlisted",
                Privacy::Public => "public",
            },
            "selfDeclaredMadeForKids": false,
        });
        if req.declare_synthetic_content {
            // YouTube's "altered or synthetic content" flag lives under
            // contentDetails -> contentRating / via the "altered content"
            // checkbox. Exact field name depends on rollout state; we set
            // a marker and the orchestrator can patch via the disclosure
            // endpoint after insert.
            snippet["description"] = serde_json::Value::String(format!(
                "{}\n\nDisclosure: This audio and visuals were generated with AI.",
                req.spec.youtube.description
            ));
        }
        if let Some(publish_at) = req.scheduled_publish_at {
            status["publishAt"] = serde_json::Value::String(publish_at.to_rfc3339());
            status["privacyStatus"] = serde_json::Value::String("private".into());
        }
        let metadata = serde_json::json!({
            "snippet": snippet,
            "status": status,
        });

        info!(
            file_size,
            video_path = %video_path.display(),
            "initiating resumable upload"
        );

        // Step 1: initiate resumable upload (POST returns upload URL in Location header).
        let init_resp = self
            .http
            .post(VIDEOS_INSERT_URL)
            .query(&[("uploadType", "resumable"), ("part", "snippet,status")])
            .bearer_auth(&access)
            .header("X-Upload-Content-Type", "video/mp4")
            .header("X-Upload-Content-Length", file_size.to_string())
            .json(&metadata)
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("initiate upload: {e}")))?;

        if !init_resp.status().is_success() {
            let st = init_resp.status();
            let text = init_resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Youtube(format!(
                "initiate upload {st}: {text}"
            )));
        }

        let upload_url = init_resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| NightdriveError::Youtube("missing Location on initiate".into()))?
            .to_string();

        debug!(%upload_url, "got resumable upload URL");

        // Step 2: PUT the file in 8 MB chunks. Each chunk PUT either receives 308
        // (Resume Incomplete — more chunks expected, with `Range` header confirming
        // what landed) or 200/201 (final — server returns the video resource). On
        // chunk failure we query upload status (a PUT with `Content-Range: bytes */N`)
        // to find out what YouTube actually has, then continue from there. See
        // `upload_in_chunks` for the per-chunk retry logic.
        let video = self
            .upload_in_chunks(&upload_url, &video_path, file_size, DEFAULT_CHUNK_SIZE)
            .await?;

        info!(video_id = %video.id, "video uploaded");
        Ok(UploadResult {
            video_id: video.id,
            upload_url,
        })
    }

    #[instrument(skip(self), fields(thumb = %thumb_path.display()))]
    async fn set_thumbnail(&self, video_id: &str, thumb_path: &Path) -> NightdriveResult<()> {
        let access = self.access_token().await?;
        let body = tokio::fs::read(thumb_path)
            .await
            .map_err(|e| NightdriveError::Io {
                path: thumb_path.display().to_string(),
                source: e,
            })?;
        let resp = self
            .http
            .post(THUMBNAILS_SET_URL)
            .query(&[("videoId", video_id)])
            .bearer_auth(&access)
            .header("Content-Type", "image/jpeg")
            .body(body)
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("thumbnail send: {e}")))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Youtube(format!(
                "thumbnail set {st}: {text}"
            )));
        }
        info!("thumbnail set");
        Ok(())
    }
}

// =============================================================================
// Chunked PUT + resume
// =============================================================================

impl YoutubeClient {
    /// PUT the file at `path` to `upload_url` in `chunk_size`-byte chunks.
    /// Returns the parsed video resource on final 200/201, or the relevant
    /// `NightdriveError::Youtube` after exhausting per-chunk retries.
    #[instrument(skip(self), fields(file_size = total_size, chunk_size))]
    async fn upload_in_chunks(
        &self,
        upload_url: &str,
        path: &Path,
        total_size: u64,
        chunk_size: u64,
    ) -> NightdriveResult<VideoResource> {
        if total_size == 0 {
            return Err(NightdriveError::Youtube(
                "cannot upload zero-byte file".into(),
            ));
        }

        let mut file = tokio::fs::File::open(path).await.map_err(|e| NightdriveError::Io {
            path: path.display().to_string(),
            source: e,
        })?;

        let mut offset: u64 = 0;
        let mut chunk_buf: Vec<u8> = Vec::with_capacity(chunk_size as usize);

        loop {
            let remaining = total_size - offset;
            let this_chunk_len = remaining.min(chunk_size);
            let end_inclusive = offset + this_chunk_len - 1;

            file.seek(SeekFrom::Start(offset)).await.map_err(|e| NightdriveError::Io {
                path: path.display().to_string(),
                source: e,
            })?;
            chunk_buf.resize(this_chunk_len as usize, 0);
            file.read_exact(&mut chunk_buf).await.map_err(|e| NightdriveError::Io {
                path: path.display().to_string(),
                source: e,
            })?;

            debug!(
                offset,
                end_inclusive,
                len = this_chunk_len,
                pct = (end_inclusive + 1) * 100 / total_size,
                "PUT chunk"
            );

            match self
                .put_chunk_with_retry(upload_url, &chunk_buf, offset, end_inclusive, total_size)
                .await?
            {
                ChunkOutcome::Complete(resource) => {
                    debug!(video_id = %resource.id, "upload complete");
                    return Ok(resource);
                }
                ChunkOutcome::ResumeAt { next_byte } => {
                    if next_byte <= offset {
                        // Server reported it has _less_ than we just sent — pathological,
                        // bail rather than loop forever.
                        return Err(NightdriveError::Youtube(format!(
                            "server reported next_byte={next_byte} <= current offset={offset}; refusing to loop"
                        )));
                    }
                    offset = next_byte;
                }
            }
        }
    }

    /// PUT one chunk with up to [`PER_CHUNK_MAX_ATTEMPTS`] attempts and
    /// exponential backoff. Between failed attempts, queries the server for
    /// its current accepted-bytes offset — if it's past `start`, the chunk
    /// is treated as accepted and we resume from the server's offset (avoids
    /// re-sending bytes the server already has).
    async fn put_chunk_with_retry(
        &self,
        upload_url: &str,
        chunk: &[u8],
        start: u64,
        end_inclusive: u64,
        total: u64,
    ) -> NightdriveResult<ChunkOutcome> {
        let mut last_err: Option<NightdriveError> = None;
        for attempt in 1..=PER_CHUNK_MAX_ATTEMPTS {
            match self.put_chunk(upload_url, chunk, start, end_inclusive, total).await {
                Ok(outcome) => return Ok(outcome),
                Err(e) if attempt < PER_CHUNK_MAX_ATTEMPTS => {
                    warn!(attempt, max = PER_CHUNK_MAX_ATTEMPTS, error = %e, "chunk PUT failed");
                    let backoff = Duration::from_millis(500 * 2u64.pow(attempt - 1));
                    tokio::time::sleep(backoff).await;

                    // Before re-PUT, ask YouTube what bytes it has. If it has more than
                    // `start` already, our chunk landed (possibly only partially) — skip
                    // re-sending and resume from the server's offset.
                    if let Ok(Some(actual_next)) = self.query_upload_offset(upload_url, total).await {
                        if actual_next > start {
                            warn!(
                                actual_next,
                                start,
                                "server has more bytes than we expected; skipping chunk re-send"
                            );
                            return Ok(ChunkOutcome::ResumeAt { next_byte: actual_next });
                        }
                    }
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| NightdriveError::Youtube("retry budget exhausted".into())))
    }

    async fn put_chunk(
        &self,
        upload_url: &str,
        chunk: &[u8],
        start: u64,
        end_inclusive: u64,
        total: u64,
    ) -> NightdriveResult<ChunkOutcome> {
        let content_range = format!("bytes {start}-{end_inclusive}/{total}");
        let resp = self
            .http
            .put(upload_url)
            .header(reqwest::header::CONTENT_TYPE, "video/mp4")
            .header(reqwest::header::CONTENT_RANGE, &content_range)
            .body(chunk.to_vec())
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("PUT chunk: {e}")))?;

        let status = resp.status();
        match status.as_u16() {
            200 | 201 => {
                let resource: VideoResource = resp
                    .json()
                    .await
                    .map_err(|e| NightdriveError::Youtube(format!("decode final: {e}")))?;
                Ok(ChunkOutcome::Complete(resource))
            }
            308 => {
                // Resume Incomplete. The `Range` header reports bytes the server
                // already has, format `bytes=0-N` (N inclusive). If absent the
                // server has received nothing yet.
                let next_byte = parse_range_next_byte(resp.headers())?
                    .unwrap_or(0);
                Ok(ChunkOutcome::ResumeAt { next_byte })
            }
            // 5xx is transient — let put_chunk_with_retry handle it.
            500..=599 => {
                let text = resp.text().await.unwrap_or_default();
                Err(NightdriveError::Youtube(format!(
                    "transient PUT chunk {status}: {text}"
                )))
            }
            _ => {
                let text = resp.text().await.unwrap_or_default();
                Err(NightdriveError::Youtube(format!(
                    "PUT chunk {status}: {text}"
                )))
            }
        }
    }

    /// Ask YouTube what byte offset it has for this upload session, without
    /// sending any new bytes. Used by [`Self::put_chunk_with_retry`] before
    /// re-trying a failed chunk PUT, and by callers that want to check
    /// resume status before continuing a long-lived upload.
    ///
    /// Returns the byte offset the server expects next (i.e. `last_received + 1`),
    /// or `None` if the server has accepted nothing yet (no `Range` header on
    /// the 308 response).
    pub async fn query_upload_offset(
        &self,
        upload_url: &str,
        total: u64,
    ) -> NightdriveResult<Option<u64>> {
        let content_range = format!("bytes */{total}");
        let resp = self
            .http
            .put(upload_url)
            .header(reqwest::header::CONTENT_RANGE, &content_range)
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .body(Vec::<u8>::new())
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("status query: {e}")))?;

        let status = resp.status();
        match status.as_u16() {
            308 => Ok(parse_range_next_byte(resp.headers())?),
            200 | 201 => {
                // Already done. Caller should treat this as "upload already complete."
                Ok(Some(total))
            }
            _ => {
                let text = resp.text().await.unwrap_or_default();
                Err(NightdriveError::Youtube(format!(
                    "status query {status}: {text}"
                )))
            }
        }
    }
}

/// Parse the `Range: bytes=0-N` header that YouTube returns on a 308 response.
/// Returns `next_byte = N + 1`, or `None` if the header is absent.
fn parse_range_next_byte(
    headers: &reqwest::header::HeaderMap,
) -> NightdriveResult<Option<u64>> {
    let Some(range_value) = headers.get(reqwest::header::RANGE) else {
        return Ok(None);
    };
    let range_str = range_value
        .to_str()
        .map_err(|e| NightdriveError::Youtube(format!("Range header not utf8: {e}")))?;
    // Format: "bytes=0-N"  →  split off the last byte after '-'.
    let last_part = range_str.split('-').nth(1).ok_or_else(|| {
        NightdriveError::Youtube(format!("malformed Range header: {range_str:?}"))
    })?;
    let last: u64 = last_part
        .trim()
        .parse()
        .map_err(|e| NightdriveError::Youtube(format!("parse Range last byte: {e}")))?;
    Ok(Some(last + 1))
}

// =============================================================================
// videos.update + delete
// =============================================================================

impl YoutubeClient {
    /// Patch an existing video resource. Any field left `None` in [`VideoUpdate`]
    /// is left untouched on the server.
    ///
    /// **API quirk:** videos.update has PUT semantics on each `part`, not PATCH —
    /// when you touch `snippet` (any of title/description/tags/category), YouTube
    /// requires the *full* snippet object including `title` + `categoryId` or it
    /// returns 400 invalidTitle. To paper over that, this method fetches the
    /// existing video's snippet first and merges the [`VideoUpdate`] fields on top
    /// of it. `status` doesn't have the same problem — only `privacyStatus` is
    /// required, so partial PUT works there.
    ///
    /// **Note on "altered or synthetic content" disclosure:** as of the YouTube
    /// Data API v3 surface stable through early 2026, the API does NOT expose a
    /// writable field for the altered-content checkbox — it's a creator-studio-only
    /// affordance. The honest path is what [`YoutubeUploader::upload_video`] already
    /// does: append the disclosure sentence to the description so listeners see it.
    /// This method intentionally does NOT try to forge a synthetic-content field,
    /// because that would silently no-op against the API and create the illusion
    /// of compliance.
    #[instrument(skip(self), fields(video_id))]
    pub async fn update_video(
        &self,
        video_id: &str,
        update: VideoUpdate,
    ) -> NightdriveResult<()> {
        let access = self.access_token().await?;

        // If we're touching snippet partially, fetch the existing snippet first so
        // we can merge the diff on top of it (PUT semantics, see the docstring).
        let touching_snippet = update.title.is_some()
            || update.description.is_some()
            || update.tags.is_some()
            || update.category_id.is_some();

        let mut snippet = if touching_snippet {
            self.fetch_video_snippet(video_id, &access).await?
        } else {
            serde_json::Map::new()
        };

        if let Some(title) = update.title {
            snippet.insert("title".into(), serde_json::Value::String(title));
        }
        if let Some(description) = update.description {
            snippet.insert("description".into(), serde_json::Value::String(description));
        }
        if let Some(tags) = update.tags {
            snippet.insert(
                "tags".into(),
                serde_json::Value::Array(
                    tags.into_iter().map(serde_json::Value::String).collect(),
                ),
            );
        }
        if let Some(category_id) = update.category_id {
            snippet.insert("categoryId".into(), serde_json::Value::String(category_id));
        }

        let mut status = serde_json::Map::new();
        if let Some(privacy) = update.privacy {
            status.insert(
                "privacyStatus".into(),
                serde_json::Value::String(
                    match privacy {
                        Privacy::Private => "private",
                        Privacy::Unlisted => "unlisted",
                        Privacy::Public => "public",
                    }
                    .to_string(),
                ),
            );
        }
        if let Some(publish_at) = update.scheduled_publish_at {
            status.insert(
                "publishAt".into(),
                serde_json::Value::String(publish_at.to_rfc3339()),
            );
            status.insert("privacyStatus".into(), serde_json::Value::String("private".into()));
        }

        // Build the parts query string from which sections we're touching.
        let mut parts: Vec<&str> = Vec::with_capacity(2);
        if !snippet.is_empty() {
            parts.push("snippet");
        }
        if !status.is_empty() {
            parts.push("status");
        }
        if parts.is_empty() {
            return Err(NightdriveError::Youtube("update_video called with no changes".into()));
        }

        let mut body = serde_json::Map::new();
        body.insert("id".into(), serde_json::Value::String(video_id.to_string()));
        if !snippet.is_empty() {
            body.insert("snippet".into(), serde_json::Value::Object(snippet));
        }
        if !status.is_empty() {
            body.insert("status".into(), serde_json::Value::Object(status));
        }

        let resp = self
            .http
            .put(VIDEOS_UPDATE_URL)
            .query(&[("part", parts.join(","))])
            .bearer_auth(&access)
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("videos.update: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Youtube(format!(
                "videos.update {status}: {text}"
            )));
        }
        info!(video_id, "video updated");
        Ok(())
    }

    /// Fetch just the `snippet` part of a video resource as a JSON object map.
    /// Used by [`Self::update_video`] to merge a partial diff on top of the
    /// current state before sending videos.update (which has PUT semantics on
    /// each part — see [`Self::update_video`]'s docstring).
    async fn fetch_video_snippet(
        &self,
        video_id: &str,
        access: &str,
    ) -> NightdriveResult<serde_json::Map<String, serde_json::Value>> {
        let resp = self
            .http
            .get(VIDEOS_UPDATE_URL)
            .query(&[("id", video_id), ("part", "snippet")])
            .bearer_auth(access)
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("videos.list (snippet): {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Youtube(format!(
                "videos.list (snippet) {status}: {text}"
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("decode list resp: {e}")))?;
        let snippet = body
            .get("items")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|item| item.get("snippet"))
            .and_then(|s| s.as_object())
            .cloned()
            .ok_or_else(|| {
                NightdriveError::Youtube(format!(
                    "videos.list returned no snippet for id={video_id}"
                ))
            })?;
        Ok(snippet)
    }

    /// Delete a video. Primarily for witness-test cleanup; production code rarely
    /// deletes its own uploads. YouTube returns 204 on success.
    #[instrument(skip(self), fields(video_id))]
    pub async fn delete_video(&self, video_id: &str) -> NightdriveResult<()> {
        let access = self.access_token().await?;
        let resp = self
            .http
            .delete(VIDEOS_UPDATE_URL)
            .query(&[("id", video_id)])
            .bearer_auth(&access)
            .send()
            .await
            .map_err(|e| NightdriveError::Youtube(format!("videos.delete: {e}")))?;

        let status = resp.status();
        if status.as_u16() == 204 || status.is_success() {
            info!(video_id, "video deleted");
            Ok(())
        } else {
            let text = resp.text().await.unwrap_or_default();
            Err(NightdriveError::Youtube(format!(
                "videos.delete {status}: {text}"
            )))
        }
    }
}
