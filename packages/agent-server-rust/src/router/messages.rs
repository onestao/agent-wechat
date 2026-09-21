use axum::{
    extract::{Path, Query},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::context::create_context;
use crate::db::get_db;
use crate::execution::run_execution_loop;
use crate::ia::types::{MediaResult, Message, SendResult, SubscriptionEvent};
use crate::plans::send_message::{SendMessageParams, SendMessagePlan};
use crate::sessions::manager::get_session;
use crate::tools::wechat_db::{find_wechat_pid, list_account_dbs};
use crate::tools::wechat_keys::{extract_keys_async, get_image_keys, get_stored_keys, store_keys};
use crate::tools::wechat_media::{get_message_media, lookup_message_raw};
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
) -> Json<Vec<Message>> {
    let session = match get_session("default") {
        Some(s) => s,
        None => return Json(Vec::new()),
    };
    let logged_in_user = match &session.logged_in_user {
        Some(u) => u.clone(),
        None => return Json(Vec::new()),
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
        return Json(Vec::new());
    }

    Json(wechat_messages::list_messages(
        &logged_in_user,
        &keys,
        &chat_id,
        params.limit,
        params.offset,
    ))
}

#[derive(Deserialize, Default)]
pub struct MediaParams {
    #[serde(default)]
    pub raw: bool,
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
                resp.headers_mut().insert("x-media-status", axum::http::HeaderValue::from_static("unsupported"));
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
                }).into_response()
            };
        }
    };
    let logged_in_user = match &session.logged_in_user {
        Some(u) => u.clone(),
        None => {
            return if params.raw {
                let mut resp = (axum::http::StatusCode::NOT_FOUND, "unsupported").into_response();
                resp.headers_mut().insert("x-media-status", axum::http::HeaderValue::from_static("unsupported"));
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
                }).into_response()
            };
        }
    };

    let mut keys = {
        let db = get_db();
        get_stored_keys(&db, &session.id, &logged_in_user)
    };

    // 1. Determine message type FIRST from message DB.
    let (local_type, _create_time, _content) = match lookup_message_raw(&logged_in_user, &keys, &chat_id, local_id) {
        Some(t) => t,
        None => {
            return if params.raw {
                let mut resp = (axum::http::StatusCode::NOT_FOUND, "unsupported").into_response();
                resp.headers_mut().insert("x-media-status", axum::http::HeaderValue::from_static("unsupported"));
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
                }).into_response()
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
    ).await {
        return if params.raw {
            let mut resp = (axum::http::StatusCode::ACCEPTED, "pending").into_response();
            resp.headers_mut().insert("x-media-status", axum::http::HeaderValue::from_static("pending"));
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
            }).into_response()
        };
    }

    let image_keys = {
        let db = get_db();
        get_image_keys(&db, &session.id, &logged_in_user)
    };

    let media = get_message_media(
        &logged_in_user,
        &keys,
        &chat_id,
        local_id,
        image_keys,
    );

    if !params.raw {
        return Json(media).into_response();
    }

    if media.media_type == "unsupported" {
        let mut resp = (axum::http::StatusCode::NOT_FOUND, "unsupported").into_response();
        resp.headers_mut().insert("x-media-status", axum::http::HeaderValue::from_static("unsupported"));
        return resp;
    }

    if media.media_type == "pending" {
        let mut resp = (axum::http::StatusCode::ACCEPTED, "pending").into_response();
        resp.headers_mut().insert("x-media-status", axum::http::HeaderValue::from_static("pending"));
        return resp;
    }

    // 1. URL-backed sticker/media:
    if let Some(ref url) = media.url {
        let mut resp = axum::response::Response::new(axum::body::Body::empty());
        let headers = resp.headers_mut();
        if let Ok(val) = axum::http::HeaderValue::from_str(url) {
            headers.insert("x-media-url", val);
        }
        headers.insert("x-media-status", axum::http::HeaderValue::from_static("ready"));
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
                axum::http::HeaderValue::from_str(mime).unwrap_or(axum::http::HeaderValue::from_static("application/octet-stream")),
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
            headers.insert("x-media-status", axum::http::HeaderValue::from_static("ready"));
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
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64).unwrap_or_default();
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
            axum::http::HeaderValue::from_str(mime).unwrap_or(axum::http::HeaderValue::from_static("application/octet-stream")),
        );
        let disp = format!("inline; filename=\"{}\"", media.filename);
        if let Ok(val) = axum::http::HeaderValue::from_str(&disp) {
            headers.insert(axum::http::header::CONTENT_DISPOSITION, val);
        }
        headers.insert("x-media-status", axum::http::HeaderValue::from_static("ready"));
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
    resp.headers_mut().insert("x-media-status", axum::http::HeaderValue::from_static("pending"));
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

    // Decode base64 file to temp file
    let mut file_path: Option<String> = None;
    if let Some(ref f) = input.file {
        // Sanitize filename: keep ASCII alphanumerics, dot, hyphen, underscore;
        // replace everything else (including CJK) with underscore so the temp
        // path stays portable across locales.  The dot is preserved so that
        // file extensions survive (e.g. "遗憾.pdf" → "__.pdf"); the mangled
        // stem is acceptable since this is a transient temp path.
        let safe_name: String = f
            .filename
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = format!(
            "/tmp/send_file_{}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            safe_name
        );
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &f.data) {
            Ok(bytes) => match std::fs::write(&path, &bytes) {
                Ok(_) => {
                    file_path = Some(path);
                }
                Err(e) => {
                    return Json(SendResult {
                        success: false,
                        error: Some(format!("Failed to write temp file: {e}")),
                    });
                }
            },
            Err(e) => {
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

    // Clean up temp files
    if let Some(p) = &image_path {
        let _ = std::fs::remove_file(p);
    }
    if let Some(p) = &file_path {
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

    let _guard = match tokio::time::timeout(std::time::Duration::from_secs(5), VOICE_KEY_EXTRACTION_MUTEX.lock()).await {
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
        ).await;

        assert_eq!(res, Ok(false));
        assert_eq!(counter.load(Ordering::SeqCst), 0, "image with missing unrelated media key must not trigger extraction");
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
        ).await;

        assert_eq!(res, Ok(false));
        assert_eq!(counter.load(Ordering::SeqCst), 0, "sticker with missing unrelated media key must not trigger extraction");
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
        ).await;

        assert_eq!(res, Ok(false));
        assert_eq!(counter.load(Ordering::SeqCst), 0, "video must not trigger extraction");
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
        ).await;

        assert_eq!(res, Ok(false));
        assert_eq!(counter.load(Ordering::SeqCst), 0, "file must not trigger extraction");
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
        ).await;

        assert_eq!(res, Ok(true));
        assert_eq!(counter.load(Ordering::SeqCst), 1, "voice missing media key must extract exactly once");
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
                ).await
            }));
        }

        for h in handles {
            let res = h.await.unwrap();
            assert!(res.is_ok());
        }

        assert_eq!(counter.load(Ordering::SeqCst), 1, "concurrent voice requests must extract exactly once");
    }
}
