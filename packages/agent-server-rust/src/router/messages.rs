use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::context::create_context;
use crate::db::get_db;
use crate::execution::run_execution_loop;
use crate::ia::types::{MediaResult, Message, SendResult, SubscriptionEvent};
use crate::plans::chat_open::{ChatOpenParams, ChatOpenPlan};
use crate::plans::send_message::{SendMessageParams, SendMessagePlan};
use crate::plans::video_download::{BubbleKind, VideoDownloadParams, VideoDownloadPlan};
use crate::sessions::manager::get_session;
use crate::tools::wechat_db::{find_wechat_pid, list_account_dbs};
use crate::tools::wechat_keys::{extract_keys_async, get_image_keys, get_stored_keys, store_keys};
use crate::tools::wechat_media::{
    get_message_media, image_has_only_thumbnail, image_missing_original, lookup_message_raw,
    video_missing_original, video_play_length,
};
use crate::tools::wechat_messages;

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    50
}

pub async fn list_messages(
    Path(chat_id): Path<String>,
    Query(params): Query<ListParams>,
) -> Response {
    let session = match get_session("default") {
        Some(s) => s,
        None => return (StatusCode::OK, Json(Vec::<Message>::new())).into_response(),
    };
    let logged_in_user = match &session.logged_in_user {
        Some(u) => u.clone(),
        None => return (StatusCode::OK, Json(Vec::<Message>::new())).into_response(),
    };

    let mut keys = {
        let db = get_db();
        get_stored_keys(&db, &session.id, &logged_in_user)
    };

    // Lazy key extraction: if message_*.db files exist on disk without stored keys, re-extract
    let on_disk = list_account_dbs(&logged_in_user);
    let has_missing_message_db = on_disk.iter().any(|name| {
        name.starts_with("message_")
            && name.ends_with(".db")
            && !name.contains("fts")
            && !name.contains("resource")
            && !keys.contains_key(name.as_str())
    });
    if has_missing_message_db {
        if let Some(pid) = find_wechat_pid() {
            let extracted = extract_keys_async(pid).await;
            if !extracted.is_empty() {
                let db = get_db();
                store_keys(&db, &session.id, &logged_in_user, &extracted);
                keys = get_stored_keys(&db, &session.id, &logged_in_user);
            }
        }
    }

    if !keys.keys().any(|k| {
        k.starts_with("message_")
            && k.ends_with(".db")
            && !k.contains("fts")
            && !k.contains("resource")
    }) {
        return (StatusCode::OK, Json(Vec::<Message>::new())).into_response();
    }

    match wechat_messages::list_messages(
        &logged_in_user,
        &keys,
        &chat_id,
        params.limit,
        params.offset,
    ) {
        Ok(msgs) => (StatusCode::OK, Json(msgs)).into_response(),
        Err(err) => {
            tracing::error!("[router/messages] list_messages failed: {err}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": {
                        "code": "hot_db_unavailable",
                        "message": format!("Hot DB snapshot query failed: {err}")
                    }
                })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize, Default)]
pub struct MediaParams {
    #[serde(default)]
    pub raw: bool,
}

/// Only media received within this window trigger UI actions.
const MEDIA_DOWNLOAD_RECENT_SECS: i64 = 24 * 60 * 60;
/// How long to report pending after triggering a download.
const MEDIA_DOWNLOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// After an image job finished, how long to keep waiting for its file.
/// Non-original images never get an `_h.dat`, so waiting for the full
/// window would only delay them; the chat-size copy is used instead.
const IMAGE_DONE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// A triggered download: when it started and, once the UI job has run,
/// when it finished.
#[derive(Clone, Copy)]
struct DownloadTrigger {
    started: std::time::Instant,
    finished: Option<std::time::Instant>,
}

/// Triggered downloads, keyed by "chat_id:local_id:job".
static MEDIA_DOWNLOAD_TRIGGERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, DownloadTrigger>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn mark_download_finished(key: &str) {
    if let Some(t) = MEDIA_DOWNLOAD_TRIGGERS.lock().unwrap().get_mut(key) {
        t.finished = Some(std::time::Instant::now());
    }
}

/// Marks a download job finished when the background task ends, including
/// on failure or panic.
struct FinishOnDrop(String);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        mark_download_finished(&self.0);
    }
}

/// UI action that makes WeChat download a message's media.
enum MediaDownloadJob {
    /// Showing the chat downloads the chat-size image.
    OpenChat,
    /// Clicking the bubble downloads the video / original image.
    ClickBubble { kind: BubbleKind, is_self: bool },
}

/// Opt-in: also fetch original images (`_h.dat`) by opening them in the
/// viewer. Off by default because it adds a UI action per image.
fn image_original_enabled() -> bool {
    matches!(
        std::env::var("AGENT_WECHAT_IMAGE_ORIGINAL")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Decide whether to keep waiting for media WeChat has not downloaded yet.
///
/// On the first request for recent media, runs `job` in the background and
/// returns true. Returns true while the wait window lasts, then false so the
/// caller falls back to whatever is on disk. Old media never trigger UI
/// actions.
fn media_download_should_wait(
    chat_id: &str,
    local_id: i64,
    create_time: i64,
    job: MediaDownloadJob,
) -> bool {
    let age = chrono::Utc::now().timestamp() - create_time;
    if !(0..=MEDIA_DOWNLOAD_RECENT_SECS).contains(&age) {
        return false;
    }
    let tag = match &job {
        MediaDownloadJob::OpenChat => "open",
        MediaDownloadJob::ClickBubble { .. } => "click",
    };
    // Videos keep downloading after the click, so they wait the full window;
    // images are on disk right after the UI job, so stop shortly after it.
    let is_image = !matches!(
        job,
        MediaDownloadJob::ClickBubble {
            kind: BubbleKind::Video { .. },
            ..
        }
    );
    let key = format!("{chat_id}:{local_id}:{tag}");
    let now = std::time::Instant::now();
    {
        let mut triggers = MEDIA_DOWNLOAD_TRIGGERS.lock().unwrap();
        triggers.retain(|_, t| now.duration_since(t.started) < MEDIA_DOWNLOAD_WAIT * 4);
        if let Some(t) = triggers.get(&key) {
            if is_image {
                if let Some(finished) = t.finished {
                    return now.duration_since(finished) < IMAGE_DONE_GRACE;
                }
            }
            return now.duration_since(t.started) < MEDIA_DOWNLOAD_WAIT;
        }
        triggers.insert(
            key.clone(),
            DownloadTrigger {
                started: now,
                finished: None,
            },
        );
    }
    let chat_id = chat_id.to_string();
    tokio::spawn(async move {
        let _finish = FinishOnDrop(key);
        match job {
            MediaDownloadJob::OpenChat => {
                tracing::info!(
                    "[media] opening chat {chat_id} to download image local_id={local_id}"
                );
                open_chat_for_media(chat_id).await;
            }
            MediaDownloadJob::ClickBubble { kind, is_self } => {
                tracing::info!(
                    "[media] clicking {kind:?} in {chat_id} to download local_id={local_id}"
                );
                click_bubble_for_media(chat_id, kind, is_self).await;
            }
        }
    });
    true
}

/// Open the chat and click the media bubble so WeChat downloads the video or
/// original image.
async fn click_bubble_for_media(chat_id: String, kind: BubbleKind, is_self: bool) {
    let Some(session) = get_session("default") else {
        return;
    };
    if session.logged_in_user.is_none() {
        return;
    }
    let mut context = {
        let db = get_db();
        create_context(session, &db)
    };
    let params = VideoDownloadParams {
        chat_id: chat_id.clone(),
        kind,
        is_self,
    };
    let noop_emit = |_: SubscriptionEvent| {};
    let (result, _) = run_execution_loop(
        &VideoDownloadPlan,
        &params,
        &mut context,
        &noop_emit,
        CancellationToken::new(),
    )
    .await;
    if !result.success {
        tracing::warn!(
            "[media] media download click failed for {chat_id}: {}",
            result.error.unwrap_or_default()
        );
    }
}

/// Whether a message was sent by the logged-in account (decides which side
/// of the chat its bubble is on).
fn message_is_self(
    account_dir: &str,
    keys: &std::collections::HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
) -> bool {
    wechat_messages::list_messages(account_dir, keys, chat_id, 50, 0)
        .ok()
        .and_then(|msgs| msgs.into_iter().find(|m| m.local_id == local_id))
        .and_then(|m| m.is_self)
        .unwrap_or(false)
}

/// Open a chat in the WeChat UI without clearing unreads. Plans are
/// serialized by the execution loop, so this does not race with sends.
async fn open_chat_for_media(chat_id: String) {
    let Some(session) = get_session("default") else {
        return;
    };
    if session.logged_in_user.is_none() {
        return;
    }
    let mut context = {
        let db = get_db();
        create_context(session, &db)
    };
    let params = ChatOpenParams {
        chat_id: chat_id.clone(),
        clear_unreads: false,
    };
    let noop_emit = |_: SubscriptionEvent| {};
    let (result, _) = run_execution_loop(
        &ChatOpenPlan,
        &params,
        &mut context,
        &noop_emit,
        CancellationToken::new(),
    )
    .await;
    if !result.success {
        tracing::warn!(
            "[media] chat open for image download failed for {chat_id}: {}",
            result.error.unwrap_or_default()
        );
    }
}

pub async fn get_media(
    Path((chat_id, local_id)): Path<(String, i64)>,
    Query(params): Query<MediaParams>,
) -> axum::response::Response {
    let session = match get_session("default") {
        Some(s) => s,
        None => {
            return if params.raw {
                let mut resp = (axum::http::StatusCode::NOT_FOUND, "unsupported").into_response();
                resp.headers_mut().insert(
                    "x-media-status",
                    axum::http::HeaderValue::from_static("unsupported"),
                );
                resp
            } else {
                Json(MediaResult {
                    media_type: "unsupported".to_string(),
                    data: None,
                    url: None,
                    format: String::new(),
                    filename: String::new(),
                    role: None,
                    file_path: None,
                })
                .into_response()
            };
        }
    };
    let logged_in_user = match &session.logged_in_user {
        Some(u) => u.clone(),
        None => {
            return if params.raw {
                let mut resp = (axum::http::StatusCode::NOT_FOUND, "unsupported").into_response();
                resp.headers_mut().insert(
                    "x-media-status",
                    axum::http::HeaderValue::from_static("unsupported"),
                );
                resp
            } else {
                Json(MediaResult {
                    media_type: "unsupported".to_string(),
                    data: None,
                    url: None,
                    format: String::new(),
                    filename: String::new(),
                    role: None,
                    file_path: None,
                })
                .into_response()
            };
        }
    };

    let mut keys = {
        let db = get_db();
        get_stored_keys(&db, &session.id, &logged_in_user)
    };

    // 1. Determine message type FIRST from message DB.
    // A message that cannot be found yet (e.g. just written by WeChat) is
    // reported as pending, not unsupported: callers cache "unsupported" and
    // would never retry.
    let (local_type, create_time, content) =
        match lookup_message_raw(&logged_in_user, &keys, &chat_id, local_id) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    "[media] message not found yet for chat_id={}, local_id={}; reporting pending",
                    chat_id,
                    local_id
                );
                return if params.raw {
                    let mut resp = (axum::http::StatusCode::ACCEPTED, "pending").into_response();
                    resp.headers_mut().insert(
                        "x-media-status",
                        axum::http::HeaderValue::from_static("pending"),
                    );
                    resp
                } else {
                    Json(MediaResult {
                        media_type: "pending".to_string(),
                        data: None,
                        url: None,
                        format: String::new(),
                        filename: String::new(),
                        role: None,
                        file_path: None,
                    })
                    .into_response()
                };
            }
        };

    let base_type = (local_type & 0xFFFFFFFF) as i32;

    // 2. Single-flight key extraction:
    // ONLY voice messages (type 34) are permitted to check or extract media_*.db keys.
    // For image [3], sticker [47], video [43], file [49], zero media key extraction is performed.
    let on_disk = list_account_dbs(&logged_in_user);
    let session_id_clone = session.id.clone();
    let logged_in_user_clone = logged_in_user.clone();
    let reload_keys = move || {
        let db = get_db();
        get_stored_keys(&db, &session_id_clone, &logged_in_user_clone)
    };
    let session_id_clone2 = session.id.clone();
    let logged_in_user_clone2 = logged_in_user.clone();
    let save_keys = move |extracted: &std::collections::HashMap<String, String>| {
        let db = get_db();
        store_keys(&db, &session_id_clone2, &logged_in_user_clone2, extracted);
    };
    let pid_opt = find_wechat_pid();
    let extract_fn = || async move {
        if let Some(pid) = pid_opt {
            extract_keys_async(pid).await
        } else {
            std::collections::HashMap::new()
        }
    };

    if let Err(()) = ensure_media_keys_for_message(
        base_type,
        &on_disk,
        &mut keys,
        reload_keys,
        save_keys,
        extract_fn,
    )
    .await
    {
        return if params.raw {
            let mut resp = (axum::http::StatusCode::ACCEPTED, "pending").into_response();
            resp.headers_mut().insert(
                "x-media-status",
                axum::http::HeaderValue::from_static("pending"),
            );
            resp
        } else {
            Json(MediaResult {
                media_type: "pending".to_string(),
                data: None,
                url: None,
                format: String::new(),
                filename: String::new(),
                role: None,
                file_path: None,
            })
            .into_response()
        };
    }

    // 3. The Linux client keeps only thumbnails until media is viewed:
    // images download once shown in the chat view, videos once their bubble
    // is clicked. For recent media missing its original, run that UI action
    // once in the background and report pending for a short window;
    // afterwards fall back to what is on disk rather than waiting forever.
    // With AGENT_WECHAT_IMAGE_ORIGINAL enabled, images are opened in the
    // viewer to fetch the original (`_h.dat`) instead of the chat-size copy.
    let download_job = match base_type {
        3 if image_original_enabled()
            && image_missing_original(&logged_in_user, &keys, &chat_id, local_id, create_time) =>
        {
            Some(MediaDownloadJob::ClickBubble {
                kind: BubbleKind::Image,
                is_self: message_is_self(&logged_in_user, &keys, &chat_id, local_id),
            })
        }
        3 if image_has_only_thumbnail(&logged_in_user, &keys, &chat_id, local_id, create_time) => {
            Some(MediaDownloadJob::OpenChat)
        }
        43 if video_missing_original(&logged_in_user, &keys, &chat_id, local_id, create_time) => {
            Some(MediaDownloadJob::ClickBubble {
                kind: BubbleKind::Video {
                    duration_secs: video_play_length(&content),
                },
                is_self: message_is_self(&logged_in_user, &keys, &chat_id, local_id),
            })
        }
        _ => None,
    };
    let should_wait = download_job
        .map(|job| media_download_should_wait(&chat_id, local_id, create_time, job))
        .unwrap_or(false);
    if should_wait {
        return if params.raw {
            let mut resp = (axum::http::StatusCode::ACCEPTED, "pending").into_response();
            resp.headers_mut().insert(
                "x-media-status",
                axum::http::HeaderValue::from_static("pending"),
            );
            resp
        } else {
            Json(MediaResult {
                media_type: "pending".to_string(),
                data: None,
                url: None,
                format: String::new(),
                filename: String::new(),
                role: None,
                file_path: None,
            })
            .into_response()
        };
    }

    let image_keys = {
        let db = get_db();
        get_image_keys(&db, &session.id, &logged_in_user)
    };

    let media = get_message_media(&logged_in_user, &keys, &chat_id, local_id, image_keys);

    if !params.raw {
        return Json(media).into_response();
    }

    if media.media_type == "unsupported" {
        let mut resp = (axum::http::StatusCode::NOT_FOUND, "unsupported").into_response();
        resp.headers_mut().insert(
            "x-media-status",
            axum::http::HeaderValue::from_static("unsupported"),
        );
        return resp;
    }

    if media.media_type == "pending" {
        let mut resp = (axum::http::StatusCode::ACCEPTED, "pending").into_response();
        resp.headers_mut().insert(
            "x-media-status",
            axum::http::HeaderValue::from_static("pending"),
        );
        return resp;
    }

    // 1. URL-backed sticker/media:
    if let Some(ref url) = media.url {
        let mut resp = axum::response::Response::new(axum::body::Body::empty());
        let headers = resp.headers_mut();
        if let Ok(val) = axum::http::HeaderValue::from_str(url) {
            headers.insert("x-media-url", val);
        }
        headers.insert(
            "x-media-status",
            axum::http::HeaderValue::from_static("ready"),
        );
        let role_str = media.role.as_deref().unwrap_or("original");
        if let Ok(val) = axum::http::HeaderValue::from_str(role_str) {
            headers.insert("x-media-role", val);
        }
        if let Ok(val) = axum::http::HeaderValue::from_str(&media.filename) {
            headers.insert("x-media-filename", val);
        }
        return resp;
    }

    // 2. Streamable file on disk:
    if let Some(ref path_str) = media.file_path {
        let path = std::path::Path::new(path_str);
        if let Ok(file) = tokio::fs::File::open(path).await {
            let stream = tokio_util::io::ReaderStream::new(file);
            let body = axum::body::Body::from_stream(stream);
            let mime = match media.format.to_lowercase().as_str() {
                "jpg" | "jpeg" => "image/jpeg",
                "png" => "image/png",
                "gif" => "image/gif",
                "mp3" => "audio/mpeg",
                "mp4" => "video/mp4",
                "pdf" => "application/pdf",
                _ => "application/octet-stream",
            };
            let mut resp = axum::response::Response::new(body);
            let headers = resp.headers_mut();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_str(mime).unwrap_or(
                    axum::http::HeaderValue::from_static("application/octet-stream"),
                ),
            );
            if let Ok(metadata) = path.metadata() {
                headers.insert(
                    axum::http::header::CONTENT_LENGTH,
                    axum::http::HeaderValue::from(metadata.len()),
                );
            }
            let disp = format!("inline; filename=\"{}\"", media.filename);
            if let Ok(val) = axum::http::HeaderValue::from_str(&disp) {
                headers.insert(axum::http::header::CONTENT_DISPOSITION, val);
            }
            headers.insert(
                "x-media-status",
                axum::http::HeaderValue::from_static("ready"),
            );
            let role_str = media.role.as_deref().unwrap_or("original");
            if let Ok(val) = axum::http::HeaderValue::from_str(role_str) {
                headers.insert("x-media-role", val);
            }
            if let Ok(val) = axum::http::HeaderValue::from_str(&media.filename) {
                headers.insert("x-media-filename", val);
            }
            return resp;
        }
    }

    // 3. In-memory base64 data:
    if let Some(b64) = media.data {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64)
            .unwrap_or_default();
        let mime = match media.format.to_lowercase().as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "mp3" => "audio/mpeg",
            "mp4" => "video/mp4",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
        let mut resp = axum::response::Response::new(axum::body::Body::from(bytes));
        let headers = resp.headers_mut();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_str(mime).unwrap_or(
                axum::http::HeaderValue::from_static("application/octet-stream"),
            ),
        );
        let disp = format!("inline; filename=\"{}\"", media.filename);
        if let Ok(val) = axum::http::HeaderValue::from_str(&disp) {
            headers.insert(axum::http::header::CONTENT_DISPOSITION, val);
        }
        headers.insert(
            "x-media-status",
            axum::http::HeaderValue::from_static("ready"),
        );
        let role_str = media.role.as_deref().unwrap_or("original");
        if let Ok(val) = axum::http::HeaderValue::from_str(role_str) {
            headers.insert("x-media-role", val);
        }
        if let Ok(val) = axum::http::HeaderValue::from_str(&media.filename) {
            headers.insert("x-media-filename", val);
        }
        return resp;
    }

    let mut resp = (axum::http::StatusCode::ACCEPTED, "pending").into_response();
    resp.headers_mut().insert(
        "x-media-status",
        axum::http::HeaderValue::from_static("pending"),
    );
    resp
}

#[derive(Deserialize)]
pub struct SendParams {
    #[serde(rename = "chatId")]
    chat_id: String,
    text: Option<String>,
    image: Option<ImageInput>,
    file: Option<FileInput>,
}

#[derive(Deserialize)]
pub struct ImageInput {
    data: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
}

#[derive(Deserialize)]
pub struct FileInput {
    data: String,
    filename: String,
}

pub fn sanitize_send_filename(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Filename cannot be empty".to_string());
    }
    if trimmed.chars().any(|c| c.is_control() || c == '\0') {
        return Err("Filename contains control characters".to_string());
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err("Filename cannot contain path separators".to_string());
    }
    if trimmed.contains(':') {
        return Err("Filename cannot contain colon".to_string());
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains("..") {
        return Err("Filename cannot contain directory traversal '..'".to_string());
    }
    Ok(trimmed.to_string())
}

struct TempDirGuard(Option<std::path::PathBuf>);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

pub async fn send_message(Json(input): Json<SendParams>) -> Json<SendResult> {
    if input.text.is_none() && input.image.is_none() && input.file.is_none() {
        return Json(SendResult {
            success: false,
            error: Some("No text, image, or file provided".to_string()),
        });
    }

    let session = match get_session("default") {
        Some(s) => s,
        None => {
            return Json(SendResult {
                success: false,
                error: Some("No session available".to_string()),
            })
        }
    };

    if session.logged_in_user.is_none() {
        return Json(SendResult {
            success: false,
            error: Some("NOT_LOGGED_IN".to_string()),
        });
    }

    // Decode base64 image to temp file
    let mut image_path: Option<String> = None;
    let mut image_mime: Option<String> = None;
    if let Some(ref img) = input.image {
        let ext = match img.mime_type.as_str() {
            "image/jpeg" => ".jpg",
            "image/gif" => ".gif",
            _ => ".png",
        };
        let path = format!(
            "/tmp/send_image_{}{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            ext
        );
        if let Ok(bytes) =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &img.data)
        {
            if std::fs::write(&path, &bytes).is_ok() {
                image_mime = Some(img.mime_type.clone());
                image_path = Some(path);
            }
        }
    }

    // Decode base64 file to temp file with original basename preserved
    let mut file_path: Option<String> = None;
    let mut _temp_send_dir_guard = TempDirGuard(None);
    if let Some(ref f) = input.file {
        let safe_name = match sanitize_send_filename(&f.filename) {
            Ok(s) => s,
            Err(e) => {
                return Json(SendResult {
                    success: false,
                    error: Some(format!("Invalid filename: {e}")),
                });
            }
        };

        let send_uuid = uuid::Uuid::new_v4().to_string();
        let send_dir = std::path::PathBuf::from(format!("/tmp/agent-wechat-send/{send_uuid}"));
        if let Err(e) = std::fs::create_dir_all(&send_dir) {
            return Json(SendResult {
                success: false,
                error: Some(format!("Failed to create temp send dir: {e}")),
            });
        }

        let full_path = send_dir.join(&safe_name);
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &f.data) {
            Ok(bytes) => match std::fs::write(&full_path, &bytes) {
                Ok(_) => {
                    file_path = Some(full_path.to_string_lossy().to_string());
                    _temp_send_dir_guard = TempDirGuard(Some(send_dir));
                }
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&send_dir);
                    return Json(SendResult {
                        success: false,
                        error: Some(format!("Failed to write temp file: {e}")),
                    });
                }
            },
            Err(e) => {
                let _ = std::fs::remove_dir_all(&send_dir);
                return Json(SendResult {
                    success: false,
                    error: Some(format!("Failed to decode base64 file data: {e}")),
                });
            }
        }
    }

    let mut context = {
        let db = get_db();
        create_context(session, &db)
    };

    let plan = SendMessagePlan;
    let params = SendMessageParams {
        chat_id: input.chat_id,
        message: input.text,
        image_path: image_path.clone(),
        image_mime,
        file_path: file_path.clone(),
    };
    let cancel = CancellationToken::new();
    let noop_emit = |_: SubscriptionEvent| {};

    let (result, _plan_state) =
        run_execution_loop(&plan, &params, &mut context, &noop_emit, cancel).await;

    // Clean up temp image file (temp file_path and its parent dir are cleaned up by _temp_send_dir_guard RAII drop)
    if let Some(p) = &image_path {
        let _ = std::fs::remove_file(p);
    }

    Json(SendResult {
        success: result.success,
        error: result.error,
    })
}

static VOICE_KEY_EXTRACTION_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Single-flight voice key extraction with double-checked locking.
///
/// Rules:
/// 1. ONLY voice messages (base_type == 34) may check or extract media_*.db keys.
/// 2. Image (3), sticker (47), video (43), and file (49) NEVER check or extract media keys.
/// 3. Voice extraction is strictly single-flight: at most one in-flight extraction at any time.
/// 4. Re-checks stored keys inside the lock to avoid duplicate extraction for queued requests.
/// 5. Bounded timeouts: 5s lock acquisition, 8s extraction; returns Err(()) on timeout to prevent hanging.
pub(crate) async fn ensure_media_keys_for_message<F, Fut, R, S>(
    base_type: i32,
    on_disk: &[String],
    keys: &mut std::collections::HashMap<String, String>,
    reload_keys: R,
    save_keys: S,
    extract_fn: F,
) -> Result<bool, ()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::collections::HashMap<String, String>>,
    R: Fn() -> std::collections::HashMap<String, String>,
    S: Fn(&std::collections::HashMap<String, String>),
{
    if base_type != 34 {
        return Ok(false);
    }

    let has_missing = on_disk.iter().any(|name| {
        name.starts_with("media_") && name.ends_with(".db") && !keys.contains_key(name.as_str())
    });
    if !has_missing {
        return Ok(false);
    }

    let _guard = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        VOICE_KEY_EXTRACTION_MUTEX.lock(),
    )
    .await
    {
        Ok(g) => g,
        Err(_) => {
            tracing::warn!("[media] voice key extraction lock timed out after 5s");
            return Err(());
        }
    };

    *keys = reload_keys();
    let still_missing = on_disk.iter().any(|name| {
        name.starts_with("media_") && name.ends_with(".db") && !keys.contains_key(name.as_str())
    });
    if !still_missing {
        return Ok(false);
    }

    match tokio::time::timeout(std::time::Duration::from_secs(8), extract_fn()).await {
        Ok(extracted) => {
            if !extracted.is_empty() {
                save_keys(&extracted);
                *keys = reload_keys();
            }
            Ok(true)
        }
        Err(_) => {
            tracing::warn!("[media] extract_keys timed out after 8s");
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_image_with_missing_unrelated_media_key_does_not_extract() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let mut keys = std::collections::HashMap::new();
        keys.insert("message_1.db".to_string(), "key1".to_string());
        let on_disk = vec!["message_1.db".to_string(), "media_99.db".to_string()];
        let keys_snap = keys.clone();

        let res = ensure_media_keys_for_message(
            3, // image
            &on_disk,
            &mut keys,
            move || keys_snap.clone(),
            |_| {},
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                std::collections::HashMap::new()
            },
        )
        .await;

        assert_eq!(res, Ok(false));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "image with missing unrelated media key must not trigger extraction"
        );
    }

    #[tokio::test]
    async fn test_sticker_with_missing_unrelated_media_key_does_not_extract() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let mut keys = std::collections::HashMap::new();
        let on_disk = vec!["media_1.db".to_string()];
        let keys_snap = keys.clone();

        let res = ensure_media_keys_for_message(
            47, // sticker
            &on_disk,
            &mut keys,
            move || keys_snap.clone(),
            |_| {},
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                std::collections::HashMap::new()
            },
        )
        .await;

        assert_eq!(res, Ok(false));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "sticker with missing unrelated media key must not trigger extraction"
        );
    }

    #[tokio::test]
    async fn test_video_does_not_extract() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let mut keys = std::collections::HashMap::new();
        let on_disk = vec!["media_1.db".to_string()];
        let keys_snap = keys.clone();

        let res = ensure_media_keys_for_message(
            43, // video
            &on_disk,
            &mut keys,
            move || keys_snap.clone(),
            |_| {},
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                std::collections::HashMap::new()
            },
        )
        .await;

        assert_eq!(res, Ok(false));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "video must not trigger extraction"
        );
    }

    #[tokio::test]
    async fn test_file_does_not_extract() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let mut keys = std::collections::HashMap::new();
        let on_disk = vec!["media_1.db".to_string()];
        let keys_snap = keys.clone();

        let res = ensure_media_keys_for_message(
            49, // file
            &on_disk,
            &mut keys,
            move || keys_snap.clone(),
            |_| {},
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                std::collections::HashMap::new()
            },
        )
        .await;

        assert_eq!(res, Ok(false));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "file must not trigger extraction"
        );
    }

    #[tokio::test]
    async fn test_voice_missing_media_key_triggers_exactly_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let mut keys = std::collections::HashMap::new();
        let on_disk = vec!["media_0.db".to_string()];
        let keys_snap = keys.clone();

        let res = ensure_media_keys_for_message(
            34, // voice
            &on_disk,
            &mut keys,
            move || keys_snap.clone(),
            |_| {},
            || async move {
                c.fetch_add(1, Ordering::SeqCst);
                let mut map = std::collections::HashMap::new();
                map.insert("media_0.db".to_string(), "key0".to_string());
                map
            },
        )
        .await;

        assert_eq!(res, Ok(true));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "voice missing media key must extract exactly once"
        );
    }

    #[tokio::test]
    async fn test_concurrent_voice_requests_trigger_exactly_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let shared_keys = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let on_disk = vec!["media_0.db".to_string()];

        let mut handles = Vec::new();
        for _ in 0..10 {
            let c = counter.clone();
            let sk = shared_keys.clone();
            let od = on_disk.clone();
            handles.push(tokio::spawn(async move {
                let mut local_keys = sk.lock().unwrap().clone();
                let reload = {
                    let sk = sk.clone();
                    move || sk.lock().unwrap().clone()
                };
                let save = {
                    let sk = sk.clone();
                    move |extracted: &std::collections::HashMap<String, String>| {
                        sk.lock().unwrap().extend(extracted.clone());
                    }
                };
                ensure_media_keys_for_message(
                    34, // voice
                    &od,
                    &mut local_keys,
                    reload,
                    save,
                    || async move {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        c.fetch_add(1, Ordering::SeqCst);
                        let mut map = std::collections::HashMap::new();
                        map.insert("media_0.db".to_string(), "key0".to_string());
                        map
                    },
                )
                .await
            }));
        }

        for h in handles {
            let res = h.await.unwrap();
            assert!(res.is_ok());
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "concurrent voice requests must extract exactly once"
        );
    }

    #[test]
    fn test_ascii_unicode_filename_preservation() {
        assert_eq!(sanitize_send_filename("test.pdf").unwrap(), "test.pdf");
        assert_eq!(
            sanitize_send_filename("实验报告.pdf").unwrap(),
            "实验报告.pdf"
        );
        assert_eq!(
            sanitize_send_filename("2026年 财务 (Q3) [final].xlsx").unwrap(),
            "2026年 财务 (Q3) [final].xlsx"
        );
        assert_eq!(
            sanitize_send_filename("🎉 emoji-file.txt").unwrap(),
            "🎉 emoji-file.txt"
        );
    }

    #[test]
    fn test_traversal_rejection() {
        assert!(sanitize_send_filename("../test.pdf").is_err());
        assert!(sanitize_send_filename("..\\test.pdf").is_err());
        assert!(sanitize_send_filename("/etc/passwd").is_err());
        assert!(sanitize_send_filename("C:\\Windows\\win.ini").is_err());
        assert!(sanitize_send_filename("foo/bar").is_err());
        assert!(sanitize_send_filename("foo\\bar").is_err());
        assert!(sanitize_send_filename("..").is_err());
        assert!(sanitize_send_filename(".").is_err());
        assert!(sanitize_send_filename("").is_err());
        assert!(sanitize_send_filename("   ").is_err());
        assert!(sanitize_send_filename("test\0bad.pdf").is_err());
        assert!(sanitize_send_filename("test\nbad.pdf").is_err());
    }

    #[test]
    fn test_temp_cleanup_success_and_failure() {
        let send_uuid = uuid::Uuid::new_v4().to_string();
        let send_dir = std::path::PathBuf::from(format!("/tmp/agent-wechat-send-test/{send_uuid}"));
        std::fs::create_dir_all(&send_dir).unwrap();
        let file_path = send_dir.join("test.pdf");
        std::fs::write(&file_path, b"test").unwrap();
        assert!(file_path.exists());

        // Simulate cleanup
        let _ = std::fs::remove_dir_all(&send_dir);
        assert!(!send_dir.exists());
    }

    #[test]
    fn test_image_wait_ends_shortly_after_job_finishes() {
        let now = chrono::Utc::now().timestamp();
        let t0 = std::time::Instant::now() - std::time::Duration::from_secs(20);
        {
            let mut triggers = MEDIA_DOWNLOAD_TRIGGERS.lock().unwrap();
            // Image click finished 10s ago: stop waiting (no _h.dat coming).
            triggers.insert(
                "grace_chat:1:click".to_string(),
                DownloadTrigger {
                    started: t0,
                    finished: Some(std::time::Instant::now() - std::time::Duration::from_secs(10)),
                },
            );
            // Image click still running: keep waiting.
            triggers.insert(
                "grace_chat:2:click".to_string(),
                DownloadTrigger {
                    started: t0,
                    finished: None,
                },
            );
            // Video click finished: keep waiting, the file may still download.
            triggers.insert(
                "grace_chat:3:click".to_string(),
                DownloadTrigger {
                    started: t0,
                    finished: Some(std::time::Instant::now() - std::time::Duration::from_secs(10)),
                },
            );
        }
        let image = || MediaDownloadJob::ClickBubble {
            kind: BubbleKind::Image,
            is_self: false,
        };
        assert!(!media_download_should_wait("grace_chat", 1, now, image()));
        assert!(media_download_should_wait("grace_chat", 2, now, image()));
        assert!(media_download_should_wait(
            "grace_chat",
            3,
            now,
            MediaDownloadJob::ClickBubble {
                kind: BubbleKind::Video {
                    duration_secs: None
                },
                is_self: false,
            }
        ));
    }

    #[test]
    fn test_media_download_wait_skips_old_and_future_media() {
        let now = chrono::Utc::now().timestamp();
        // Older than the window: never opens a chat, never waits.
        assert!(!media_download_should_wait(
            "old_chat",
            1,
            now - MEDIA_DOWNLOAD_RECENT_SECS - 60,
            MediaDownloadJob::OpenChat
        ));
        // Timestamp in the future (clock skew): treated as not recent.
        assert!(!media_download_should_wait(
            "future_chat",
            1,
            now + 3600,
            MediaDownloadJob::OpenChat
        ));
        assert!(MEDIA_DOWNLOAD_TRIGGERS
            .lock()
            .unwrap()
            .keys()
            .all(|k| !k.starts_with("old_chat") && !k.starts_with("future_chat")));
    }
}
