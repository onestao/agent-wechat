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
use crate::tools::wechat_media::get_message_media;
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

    // Lazy key extraction: if media_*.db files exist on disk without stored keys, extract them
    let on_disk = list_account_dbs(&logged_in_user);
    let has_missing_media = on_disk.iter().any(|name| {
        name.starts_with("media_") && name.ends_with(".db") && !keys.contains_key(name.as_str())
    });
    if has_missing_media {
        if let Some(pid) = find_wechat_pid() {
            let extracted = extract_keys_async(pid).await;
            if !extracted.is_empty() {
                let db = get_db();
                store_keys(&db, &session.id, &logged_in_user, &extracted);
                keys = get_stored_keys(&db, &session.id, &logged_in_user);
            }
        }
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
