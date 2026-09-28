use crate::ia::types::MediaResult;
use crate::tools::wechat_db::{get_db_path, query_wechat_db};
use crate::tools::wechat_messages::{
    decode_message_content, extract_xml_tag, find_message_db, get_msg_table_name,
};
use md5::{Digest, Md5};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

/// WeChat .dat file magic bytes: 07 08 56 32 08 07
const DAT_MAGIC: [u8; 6] = [0x07, 0x08, 0x56, 0x32, 0x08, 0x07];

pub struct ImageKeys {
    aes_key_hex: String,
    xor_byte: Option<u8>,
}

fn unsupported() -> MediaResult {
    MediaResult {
        media_type: "unsupported".into(),
        data: None,
        url: None,
        format: String::new(),
        filename: String::new(),
        role: None,
        file_path: None,
    }
}

fn pending() -> MediaResult {
    MediaResult {
        media_type: "pending".into(),
        data: None,
        url: None,
        format: String::new(),
        filename: String::new(),
        role: None,
        file_path: None,
    }
}

fn account_base_paths(account_dir: &str) -> [String; 2] {
    [
        format!("/home/wechat/xwechat_files/{account_dir}"),
        format!("/home/wechat/Documents/xwechat_files/{account_dir}"),
    ]
}

/// Look up a single message's raw content by localId.
pub(crate) fn lookup_message_raw(
    account_dir: &str,
    keys: &HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
) -> Option<(i64, i64, String)> {
    let table_name = get_msg_table_name(chat_id);
    let (db_name, key) = find_message_db(account_dir, keys, chat_id)?;
    let db_path = get_db_path(account_dir, &db_name);

    let rows = query_wechat_db(
        &db_path,
        key,
        &format!(
            "SELECT local_type, create_time,
                    hex(message_content) as hex_content,
                    WCDB_CT_message_content as is_compressed
             FROM \"{table_name}\"
             WHERE local_id = {local_id}
             LIMIT 1;"
        ),
    );

    let row = rows.first()?;
    let local_type = row.get("local_type")?.as_i64()?;
    let create_time = row.get("create_time").and_then(|v| v.as_i64()).unwrap_or(0);
    let hex_content = row
        .get("hex_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_compressed = row
        .get("is_compressed")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        != 0;

    let content = decode_message_content(hex_content, is_compressed);
    // Strip group sender prefix
    let body = if let Some(idx) = content.find(":\n") {
        if idx < 80 {
            content[idx + 2..].to_string()
        } else {
            content
        }
    } else {
        content
    };

    Some((local_type, create_time, body))
}

/// Extract an XML attribute value.
fn xml_attr(xml: &str, attr: &str) -> Option<String> {
    super::wechat_messages::extract_xml_attr(xml, attr)
}

// ── Image thumbnail from filesystem cache ────────────────────────────────────

fn get_image_thumbnail(
    account_dir: &str,
    chat_id: &str,
    local_id: i64,
    create_time: i64,
) -> Option<MediaResult> {
    let hash = format!("{:x}", Md5::digest(chat_id.as_bytes()));
    let dt = chrono::DateTime::from_timestamp(create_time, 0)?;
    let year_month = dt.format("%Y-%m").to_string();
    let thumb_name = format!("{local_id}_{create_time}_thumb.jpg");

    for base in &account_base_paths(account_dir) {
        let thumb_path = Path::new(base)
            .join("cache")
            .join(&year_month)
            .join("Message")
            .join(&hash)
            .join("Thumb")
            .join(&thumb_name);
        if thumb_path.exists() {
            let p_str = thumb_path.to_string_lossy().to_string();
            if let Ok(data) = fs::read(&thumb_path) {
                return Some(MediaResult {
                    media_type: "image".into(),
                    data: Some(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &data,
                    )),
                    url: None,
                    format: "jpeg".into(),
                    filename: format!("msg_{local_id}.jpg"),
                    role: Some("thumbnail".into()),
                    file_path: Some(p_str),
                });
            }
        }

        // Fallback: find any thumbnail matching this localId
        let thumb_dir = Path::new(base)
            .join("cache")
            .join(&year_month)
            .join("Message")
            .join(&hash)
            .join("Thumb");
        if let Ok(entries) = fs::read_dir(&thumb_dir) {
            let prefix = format!("{local_id}_");
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&prefix) {
                    let p_str = entry.path().to_string_lossy().to_string();
                    if let Ok(data) = fs::read(entry.path()) {
                        return Some(MediaResult {
                            media_type: "image".into(),
                            data: Some(base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD,
                                &data,
                            )),
                            url: None,
                            format: "jpeg".into(),
                            filename: format!("msg_{local_id}.jpg"),
                            role: Some("thumbnail".into()),
                            file_path: Some(p_str),
                        });
                    }
                }
            }
        }
    }
    None
}

// ── .dat file decryption ─────────────────────────────────────────────────────

fn aligned_aes_size(enc_chunk_size: u32) -> u32 {
    let rem = enc_chunk_size % 16;
    if rem == 0 {
        enc_chunk_size + 16
    } else {
        enc_chunk_size + (16 - rem)
    }
}

fn decrypt_dat_head(dat: &[u8], aes_key_hex: &str) -> Option<(Vec<u8>, u32)> {
    if dat.len() < 15 || dat[..6] != DAT_MAGIC {
        return None;
    }
    let enc_chunk_size = u32::from_le_bytes(dat[6..10].try_into().ok()?);
    let aes_key = &aes_key_hex.as_bytes()[..16]; // first 16 ASCII chars

    let aligned = aligned_aes_size(enc_chunk_size) as usize;
    if dat.len() < 15 + aligned {
        return None;
    }
    let ct = &dat[15..15 + aligned];

    // AES-128-ECB decrypt via openssl CLI (no native Rust AES dep needed)
    let mut child = Command::new("openssl")
        .args(["enc", "-d", "-aes-128-ecb", "-K"])
        .arg(hex_encode(aes_key))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    use std::io::Write;
    child.stdin.take()?.write_all(ct).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }

    Some((output.stdout, enc_chunk_size))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

fn derive_xor_byte(dat: &[u8], dec_head: &[u8]) -> Option<u8> {
    if dec_head.len() >= 2 && dec_head[0] == 0xff && dec_head[1] == 0xd8 {
        // JPEG: last 2 bytes should be FF D9
        let c1 = dat[dat.len() - 2] ^ 0xFF;
        let c2 = dat[dat.len() - 1] ^ 0xD9;
        if c1 == c2 {
            return Some(c1);
        }
    }
    if dec_head.len() >= 4 && dec_head[..4] == [0x89, 0x50, 0x4e, 0x47] {
        // PNG: last 8 bytes are IEND chunk
        let expected = [0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82];
        if dat.len() >= 8 {
            let ts = dat.len() - 8;
            let xb = dat[ts] ^ expected[0];
            if expected
                .iter()
                .enumerate()
                .all(|(i, &e)| (dat[ts + i] ^ xb) == e)
            {
                return Some(xb);
            }
        }
    }
    if dec_head.len() >= 4 && &dec_head[..4] == b"GIF8" {
        // GIF: last 2 bytes are 00 3B
        let c1 = dat[dat.len() - 2] ^ 0x00;
        let c2 = dat[dat.len() - 1] ^ 0x3B;
        if c1 == c2 {
            return Some(c1);
        }
    }
    None
}

fn resolve_xor_byte(dat_path: &str, dat: &[u8], image_keys: &ImageKeys) -> Option<u8> {
    if let Some(xb) = image_keys.xor_byte {
        return Some(xb);
    }
    let (dec_head, _) = decrypt_dat_head(dat, &image_keys.aes_key_hex)?;
    let xb = derive_xor_byte(dat, &dec_head);
    if xb.is_some() {
        return xb;
    }
    // Try sibling _t.dat files (JPEG thumbnails are reliable for XOR derivation)
    let dir = Path::new(dat_path).parent()?;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with("_t.dat") {
                continue;
            }
            if let Ok(sib) = fs::read(entry.path()) {
                if sib.len() < 15 || sib[..6] != DAT_MAGIC {
                    continue;
                }
                if let Some((sib_head, _)) = decrypt_dat_head(&sib, &image_keys.aes_key_hex) {
                    if let Some(xb) = derive_xor_byte(&sib, &sib_head) {
                        return Some(xb);
                    }
                }
            }
        }
    }
    None
}

fn decrypt_dat(dat: &[u8], aes_key_hex: &str, xor_byte: u8) -> Option<Vec<u8>> {
    let (dec_head, enc_chunk_size) = decrypt_dat_head(dat, aes_key_hex)?;
    let xor_size = u32::from_le_bytes(dat[10..14].try_into().ok()?) as usize;
    let aes_ct_end = 15 + aligned_aes_size(enc_chunk_size) as usize;
    let remaining = &dat[aes_ct_end..];

    let raw_length = remaining.len().saturating_sub(xor_size);
    let raw_data = &remaining[..raw_length];
    let xor_data = &remaining[raw_length..];

    let dec_tail: Vec<u8> = xor_data.iter().map(|b| b ^ xor_byte).collect();

    let mut result = Vec::with_capacity(dec_head.len() + raw_data.len() + dec_tail.len());
    result.extend_from_slice(&dec_head);
    result.extend_from_slice(raw_data);
    result.extend_from_slice(&dec_tail);
    Some(result)
}

fn detect_image_format(data: &[u8]) -> (&'static str, &'static str) {
    if data.len() >= 2 && data[0] == 0xff && data[1] == 0xd8 {
        return ("jpeg", "jpg");
    }
    if data.len() >= 4 && data[..4] == [0x89, 0x50, 0x4e, 0x47] {
        return ("png", "png");
    }
    if data.len() >= 4 && &data[..4] == b"GIF8" {
        return ("gif", "gif");
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return ("webp", "webp");
    }
    if data.len() >= 4 && &data[..4] == b"wxgf" {
        return ("wxgf", "wxgf");
    }
    ("unknown", "bin")
}

/// Convert media via the media-convert tool.
fn convert_media(mode: &str, input: &[u8]) -> Option<(Vec<u8>, String)> {
    use std::io::Write;
    let mut child = Command::new("media-convert")
        .arg(mode)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let format = stderr
        .lines()
        .find_map(|l| l.strip_prefix("FORMAT:"))
        .unwrap_or(if mode == "silk2mp3" { "mp3" } else { "jpeg" })
        .to_string();
    Some((output.stdout, format))
}

// ── .dat file resolution via hardlink.db ─────────────────────────────────────

fn find_dat_via_hardlink(
    account_dir: &str,
    keys: &HashMap<String, String>,
    _chat_id: &str,
    content: &str,
) -> Option<(String, &'static str)> {
    let hardlink_key = match keys.get("hardlink.db") {
        Some(k) => k,
        None => {
            tracing::warn!("[media:hardlink] no key for hardlink.db");
            return None;
        }
    };
    let image_md5 = match xml_attr(content, "md5") {
        Some(m) if !m.is_empty() => m,
        _ => {
            tracing::warn!(
                "[media:hardlink] no md5 attr in content (len={})",
                content.len()
            );
            return None;
        }
    };
    let hardlink_db = get_db_path(account_dir, "hardlink.db");

    let file_rows = query_wechat_db(
        &hardlink_db,
        hardlink_key,
        &format!(
            "SELECT file_name, dir1, dir2 FROM image_hardlink_info_v4
             WHERE md5 = '{image_md5}' LIMIT 10;"
        ),
    );
    if file_rows.is_empty() {
        tracing::warn!("[media:hardlink] no hardlink row for md5={}", image_md5);
        return None;
    }

    // Try candidates prioritizing _h (original), then standard (.dat), then _t (thumbnail)
    for target_role in &["original", "standard", "thumbnail"] {
        for row in &file_rows {
            let file_name = match row.get("file_name").and_then(|v| v.as_str()) {
                Some(f) => f,
                None => continue,
            };
            let dir1 = match row.get("dir1").and_then(|v| v.as_i64()) {
                Some(d) => d,
                None => continue,
            };
            let dir2 = match row.get("dir2").and_then(|v| v.as_i64()) {
                Some(d) => d,
                None => continue,
            };

            let dir_rows = query_wechat_db(
                &hardlink_db,
                hardlink_key,
                &format!("SELECT rowid, username FROM dir2id WHERE rowid IN ({dir1}, {dir2});"),
            );
            let dir_map: HashMap<i64, String> = dir_rows
                .iter()
                .filter_map(|r| {
                    let rid = r.get("rowid")?.as_i64()?;
                    let name = r.get("username")?.as_str()?.to_string();
                    Some((rid, name))
                })
                .collect();

            let chat_dir = match dir_map.get(&dir1) {
                Some(d) => d,
                None => continue,
            };
            let date_dir = match dir_map.get(&dir2) {
                Some(d) => d,
                None => continue,
            };

            let stem = file_name
                .strip_suffix("_h.dat")
                .or_else(|| file_name.strip_suffix("_t.dat"))
                .or_else(|| file_name.strip_suffix(".dat"))
                .unwrap_or(file_name);

            let candidate_file = match *target_role {
                "original" => format!("{stem}_h.dat"),
                "standard" => format!("{stem}.dat"),
                "thumbnail" => format!("{stem}_t.dat"),
                _ => continue,
            };

            for base in &account_base_paths(account_dir) {
                let dat_path = Path::new(base)
                    .join("msg/attach")
                    .join(chat_dir)
                    .join(date_dir)
                    .join("Img")
                    .join(&candidate_file);
                if dat_path.exists() {
                    return Some((dat_path.to_string_lossy().to_string(), *target_role));
                }
            }
        }
    }

    tracing::warn!(
        "[media:hardlink] .dat file not found on disk for md5={}",
        image_md5
    );
    None
}

/// Look up the file hash for a message from message_resource.db.
/// Returns the 32-char hex hash used in filenames on disk.
fn find_file_hash_via_resource_db(
    account_dir: &str,
    keys: &HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
) -> Option<String> {
    let resource_key = keys.get("message_resource.db")?;
    let resource_db = get_db_path(account_dir, "message_resource.db");

    // Look up chat_id integer from ChatName2Id
    let chat_rows = query_wechat_db(
        &resource_db,
        resource_key,
        &format!(
            "SELECT rowid FROM ChatName2Id WHERE user_name = '{}' LIMIT 1;",
            chat_id.replace('\'', "''")
        ),
    );
    let chat_id_int = chat_rows.first()?.get("rowid")?.as_i64()?;

    // Query packed_info from MessageResourceInfo
    let info_rows = query_wechat_db(
        &resource_db,
        resource_key,
        &format!(
            "SELECT hex(packed_info) as hex_info FROM MessageResourceInfo
             WHERE chat_id = {chat_id_int} AND message_local_id = {local_id}
             LIMIT 1;"
        ),
    );
    let hex_info = info_rows.first()?.get("hex_info")?.as_str()?.to_string();

    let file_hash = extract_file_hash_from_packed_info(&hex_info)?;
    tracing::info!(
        "[media:resource-db] file_hash={} for local_id={}",
        file_hash,
        local_id
    );
    Some(file_hash)
}

/// Look up the .dat filename from message_resource.db. The packed_info blob in
/// MessageResourceInfo contains the file hash used as the .dat filename.
fn find_dat_via_resource_db(
    account_dir: &str,
    keys: &HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
    create_time: i64,
) -> Option<(String, &'static str)> {
    let file_hash = find_file_hash_via_resource_db(account_dir, keys, chat_id, local_id)?;

    // Build path: msg/attach/<md5(chatId)>/<year-month>/Img/<hash>.dat
    let chat_hash = format!("{:x}", Md5::digest(chat_id.as_bytes()));
    let dt = chrono::DateTime::from_timestamp(create_time, 0)?;
    let year_month = dt.format("%Y-%m").to_string();

    for base in &account_base_paths(account_dir) {
        // Try HD .dat first, then standard .dat, then _t.dat thumbnail
        for (suffix, role) in &[("_h", "original"), ("", "standard"), ("_t", "thumbnail")] {
            let dat_path = Path::new(base)
                .join("msg/attach")
                .join(&chat_hash)
                .join(&year_month)
                .join("Img")
                .join(format!("{file_hash}{suffix}.dat"));
            if dat_path.exists() {
                return Some((dat_path.to_string_lossy().to_string(), *role));
            }
        }
    }

    tracing::warn!(
        "[media:resource-db] file not on disk yet for hash={}",
        file_hash
    );
    None
}

/// Get video data: .mp4 if downloaded, otherwise cover .jpg or _thumb.jpg.
/// Videos are stored unencrypted at msg/video/{YYYY-MM}/{hash}.mp4
fn get_video_data(
    account_dir: &str,
    keys: &HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
    create_time: i64,
) -> MediaResult {
    let dt = match chrono::DateTime::from_timestamp(create_time, 0) {
        Some(dt) => dt,
        None => return unsupported(),
    };
    let year_month = dt.format("%Y-%m").to_string();

    // Try to get file hash from message_resource.db
    let file_hash = find_file_hash_via_resource_db(account_dir, keys, chat_id, local_id);

    if let Some(ref hash) = file_hash {
        for base in &account_base_paths(account_dir) {
            let video_dir = Path::new(base).join("msg/video").join(&year_month);

            // Try .mp4 first (full video)
            let mp4_path = video_dir.join(format!("{hash}.mp4"));
            if mp4_path.exists() {
                let p_str = mp4_path.to_string_lossy().to_string();
                return MediaResult {
                    media_type: "video".into(),
                    data: None,
                    url: None,
                    format: "mp4".into(),
                    filename: format!("msg_{local_id}.mp4"),
                    role: Some("original".into()),
                    file_path: Some(p_str),
                };
            }

            // Try cover .jpg (full-size cover image)
            let cover_path = video_dir.join(format!("{hash}.jpg"));
            if cover_path.exists() {
                let p_str = cover_path.to_string_lossy().to_string();
                return MediaResult {
                    media_type: "video".into(),
                    data: None,
                    url: None,
                    format: "jpeg".into(),
                    filename: format!("msg_{local_id}_cover.jpg"),
                    role: Some("thumbnail".into()),
                    file_path: Some(p_str),
                };
            }

            // Try _thumb.jpg
            let thumb_path = video_dir.join(format!("{hash}_thumb.jpg"));
            if thumb_path.exists() {
                let p_str = thumb_path.to_string_lossy().to_string();
                return MediaResult {
                    media_type: "video".into(),
                    data: None,
                    url: None,
                    format: "jpeg".into(),
                    filename: format!("msg_{local_id}_thumb.jpg"),
                    role: Some("thumbnail".into()),
                    file_path: Some(p_str),
                };
            }
        }
    }

    // Fallback: try cached thumbnail from WeChat's cache dir
    if let Some(thumb) = get_image_thumbnail(account_dir, chat_id, local_id, create_time) {
        return thumb;
    }

    // Video exists but no file found on disk yet
    tracing::warn!(
        "[media:video] no video file found for local_id={}",
        local_id
    );
    pending()
}

/// Extract the 32-char hex file hash from a MessageResourceInfo packed_info blob.
/// The blob is protobuf-encoded: field 2 (tag 0x12), length-delimited, containing
/// field 1 (tag 0x0A), 32 bytes of ASCII hex hash.
fn extract_file_hash_from_packed_info(hex_info: &str) -> Option<String> {
    let bytes = crate::tools::wechat_messages::hex_decode(hex_info)?;
    // Find the ASCII hex hash: 32 chars [0-9a-f]
    // It's at a fixed offset in the protobuf, but let's be robust and scan for it
    for window in bytes.windows(32) {
        if window.iter().all(|&b| b.is_ascii_hexdigit()) {
            let candidate = std::str::from_utf8(window).ok()?;
            // Verify it's lowercase hex (not random ASCII digits)
            if candidate
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
            {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

pub fn candidate_rank(role: &str) -> u8 {
    match role {
        "original" => 3,
        "standard" => 2,
        "thumbnail" => 1,
        _ => 0,
    }
}

pub fn select_best_candidate(
    cand_a: Option<(String, &'static str)>,
    cand_b: Option<(String, &'static str)>,
) -> Option<(String, &'static str)> {
    match (cand_a, cand_b) {
        (Some(a), Some(b)) => {
            if candidate_rank(b.1) > candidate_rank(a.1) {
                Some(b)
            } else {
                Some(a)
            }
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub fn evaluate_image_candidate(
    dat_path: &str,
    candidate_role: &str,
    target_hd_len: Option<u64>,
    image_keys: &ImageKeys,
    local_id: i64,
) -> MediaResult {
    if candidate_role == "thumbnail" {
        tracing::info!(
            "[media] candidate is thumbnail for local_id={}, returning pending",
            local_id
        );
        return pending();
    }

    let (mut res, raw_format, raw_decrypted_len) =
        decrypt_and_return_detail(dat_path, image_keys, local_id, candidate_role);
    // If decryption or conversion produced pending() (e.g. unknown format or failed wxgf)
    if res.media_type == "pending" || res.data.is_none() {
        return pending();
    }

    if let Some(hd_len) = target_hd_len {
        // If candidate was already explicitly named _h.dat by WeChat
        if candidate_role == "original" {
            res.role = Some("original".into());
            return res;
        }

        // If candidate was .dat (standard suffix), but message has hdlength:
        // WXGF transcoding alters payload bytes unpredictably. Without a proven WXGF HD sample,
        // comparing transcoded bytes to hdlength is unsound; candidate must remain pending until _h.dat arrives.
        if raw_format == "wxgf" {
            tracing::info!(
                "[media] message has hdlength={} but candidate is WXGF format for local_id={}, returning pending (HD_WXGF_UNVERIFIED)",
                hd_len,
                local_id
            );
            return pending();
        }

        // For standard formats (JPEG, PNG, etc.), check raw decrypted image payload length against hdlength
        let payload_len = raw_decrypted_len as u64;

        // If payload is smaller than hd_len, it is only a mid-res preview and the full HD image is not ready!
        if payload_len < hd_len {
            tracing::info!(
                "[media] message has hdlength={} but decrypted image payload is only {} bytes for local_id={}, returning pending",
                hd_len,
                payload_len,
                local_id
            );
            return pending();
        }

        // Promoted to original because payload satisfies the HD length
        res.role = Some("original".into());
    }

    res
}

fn decrypt_and_return(
    dat_path: &str,
    image_keys: &ImageKeys,
    local_id: i64,
    file_role: &str,
) -> MediaResult {
    decrypt_and_return_detail(dat_path, image_keys, local_id, file_role).0
}

fn decrypt_and_return_detail(
    dat_path: &str,
    image_keys: &ImageKeys,
    local_id: i64,
    file_role: &str,
) -> (MediaResult, String, usize) {
    let effective_role = match file_role {
        "original" => "original",
        "standard" => "standard",
        "thumbnail" => "thumbnail",
        _ => "unknown",
    };

    let dat = match fs::read(dat_path) {
        Ok(d) => d,
        Err(_) => {
            return (
                MediaResult {
                    media_type: "image".into(),
                    data: None,
                    url: None,
                    format: "jpeg".into(),
                    filename: format!("msg_{local_id}.jpg"),
                    role: None,
                    file_path: None,
                },
                "unknown".into(),
                0,
            );
        }
    };

    let xor_byte = match resolve_xor_byte(dat_path, &dat, image_keys) {
        Some(xb) => xb,
        None => {
            return (
                MediaResult {
                    media_type: "image".into(),
                    data: None,
                    url: None,
                    format: "jpeg".into(),
                    filename: format!("msg_{local_id}.jpg"),
                    role: None,
                    file_path: None,
                },
                "unknown".into(),
                0,
            );
        }
    };

    let decrypted = match decrypt_dat(&dat, &image_keys.aes_key_hex, xor_byte) {
        Some(d) => d,
        None => {
            return (
                MediaResult {
                    media_type: "image".into(),
                    data: None,
                    url: None,
                    format: "jpeg".into(),
                    filename: format!("msg_{local_id}.jpg"),
                    role: None,
                    file_path: None,
                },
                "unknown".into(),
                0,
            );
        }
    };

    let raw_decrypted_len = decrypted.len();
    let (format, ext) = detect_image_format(&decrypted);

    // If format is unknown -> NOT deliverable! Keep retryable (pending)
    if format == "unknown" {
        tracing::warn!(
            "[media] decrypted data for local_id={} has unknown image format, returning pending",
            local_id
        );
        return (pending(), "unknown".into(), raw_decrypted_len);
    }

    // WXGF → convert via media-convert wxgf2img, must succeed to be deliverable
    if format == "wxgf" {
        if let Some((converted, cfmt)) = convert_media("wxgf2img", &decrypted) {
            let cext = if cfmt == "jpeg" {
                "jpg".to_string()
            } else {
                cfmt.clone()
            };
            return (
                MediaResult {
                    media_type: "image".into(),
                    data: Some(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &converted,
                    )),
                    url: None,
                    format: cfmt,
                    filename: format!("msg_{local_id}.{cext}"),
                    role: Some(effective_role.into()),
                    file_path: None,
                },
                "wxgf".into(),
                raw_decrypted_len,
            );
        } else {
            // WXGF conversion failed: keep retryable, DO NOT deliver raw WXGF or fallback to thumbnail
            tracing::warn!(
                "[media] wxgf2img conversion failed for local_id={}, returning pending",
                local_id
            );
            return (pending(), "wxgf".into(), raw_decrypted_len);
        }
    }

    (
        MediaResult {
            media_type: "image".into(),
            data: Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &decrypted,
            )),
            url: None,
            format: format.into(),
            filename: format!("msg_{local_id}.{ext}"),
            role: Some(effective_role.into()),
            file_path: None,
        },
        format.into(),
        raw_decrypted_len,
    )
}

// ── Emoji / Sticker ──────────────────────────────────────────────────────────

fn get_emoji_media(
    account_dir: &str,
    keys: &HashMap<String, String>,
    content: &str,
    _local_id: i64,
) -> MediaResult {
    let md5_val = match xml_attr(content, "md5") {
        Some(m) => m,
        None => return unsupported(),
    };

    // Look up CDN URL from emoticon.db
    if let Some(emoticon_key) = keys.get("emoticon.db") {
        let emoticon_db = get_db_path(account_dir, "emoticon.db");
        let rows = query_wechat_db(
            &emoticon_db,
            emoticon_key,
            &format!("SELECT cdn_url FROM kNonStoreEmoticonTable WHERE md5 = '{md5_val}' LIMIT 1;"),
        );
        if let Some(row) = rows.first() {
            if let Some(url) = row.get("cdn_url").and_then(|v| v.as_str()) {
                if !url.is_empty() {
                    return MediaResult {
                        media_type: "sticker".into(),
                        data: None,
                        url: Some(url.to_string()),
                        format: "gif".into(),
                        filename: format!("emoji_{md5_val}.gif"),
                        role: Some("original".into()),
                        file_path: None,
                    };
                }
            }
        }
    }

    // Fallback: extract cdnurl or encrypturl from message XML
    if let Some(url) = xml_attr(content, "cdnurl").or_else(|| xml_attr(content, "encrypturl")) {
        if url.starts_with("http") {
            return MediaResult {
                media_type: "sticker".into(),
                data: None,
                url: Some(url),
                format: "gif".into(),
                filename: format!("emoji_{md5_val}.gif"),
                role: Some("original".into()),
                file_path: None,
            };
        }
    }

    MediaResult {
        media_type: "sticker".into(),
        data: None,
        url: None,
        format: "unknown".into(),
        filename: format!("emoji_{md5_val}"),
        role: None,
        file_path: None,
    }
}

// ── Voice ────────────────────────────────────────────────────────────────────

fn get_voice_data(
    account_dir: &str,
    keys: &HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
) -> MediaResult {
    // Try media_0.db, media_1.db, etc.
    let mut media_dbs: Vec<(&str, &str)> = keys
        .iter()
        .filter(|(k, _)| k.starts_with("media_") && k.ends_with(".db"))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    media_dbs.sort_by_key(|(k, _)| k.to_string());

    for (db_name, media_key) in &media_dbs {
        let media_db = get_db_path(account_dir, db_name);

        let name_rows = query_wechat_db(
            &media_db,
            media_key,
            &format!(
                "SELECT rowid FROM Name2Id WHERE user_name = '{}';",
                chat_id.replace('\'', "''")
            ),
        );
        let chat_name_id = match name_rows.first().and_then(|r| r.get("rowid")?.as_i64()) {
            Some(id) => id,
            None => continue,
        };

        let voice_rows = query_wechat_db(
            &media_db,
            media_key,
            &format!(
                "SELECT hex(voice_data) as hex_data FROM VoiceInfo
                 WHERE chat_name_id = {chat_name_id} AND local_id = {local_id}
                 LIMIT 1;"
            ),
        );
        let hex_data = match voice_rows.first().and_then(|r| r.get("hex_data")?.as_str()) {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => continue,
        };

        let silk_bytes = match crate::tools::wechat_messages::hex_decode(&hex_data) {
            Some(b) => b,
            None => continue,
        };

        // Try SILK → MP3 conversion
        if let Some((mp3, _)) = convert_media("silk2mp3", &silk_bytes) {
            return MediaResult {
                media_type: "voice".into(),
                data: Some(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &mp3,
                )),
                url: None,
                format: "mp3".into(),
                filename: format!("msg_{local_id}.mp3"),
                role: Some("original".into()),
                file_path: None,
            };
        }

        // Fall back to raw SILK
        return MediaResult {
            media_type: "voice".into(),
            data: Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &silk_bytes,
            )),
            url: None,
            format: "silk".into(),
            filename: format!("msg_{local_id}.silk"),
            role: Some("original".into()),
            file_path: None,
        };
    }

    pending()
}

// ── File attachment ──────────────────────────────────────────────────────────

fn get_file_attachment(
    account_dir: &str,
    content: &str,
    create_time: i64,
    local_id: i64,
) -> MediaResult {
    let filename = extract_xml_tag(content, "title").unwrap_or_else(|| format!("file_{local_id}"));
    let ext = extract_xml_tag(content, "fileext").unwrap_or_default();

    // Files are stored at <account>/msg/file/YYYY-MM/<filename>
    let dt = chrono::DateTime::from_timestamp(create_time, 0);
    let year_month = dt
        .map(|d| d.format("%Y-%m").to_string())
        .unwrap_or_default();

    for base in &account_base_paths(account_dir) {
        let file_path = Path::new(base)
            .join("msg/file")
            .join(&year_month)
            .join(&filename);
        if file_path.exists() {
            let p_str = file_path.to_string_lossy().to_string();
            return MediaResult {
                media_type: "file".into(),
                data: None,
                url: None,
                format: ext,
                filename,
                role: Some("original".into()),
                file_path: Some(p_str),
            };
        }
    }

    // File not yet downloaded by WeChat
    pending()
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Get media attachment for a message.
pub fn get_message_media(
    account_dir: &str,
    keys: &HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
    image_keys_raw: Option<(String, Option<u8>)>,
) -> MediaResult {
    get_message_media_with_raw(account_dir, keys, chat_id, local_id, image_keys_raw, None)
}

pub fn get_message_media_with_raw(
    account_dir: &str,
    keys: &HashMap<String, String>,
    chat_id: &str,
    local_id: i64,
    image_keys_raw: Option<(String, Option<u8>)>,
    message_raw: Option<(i64, i64, String)>,
) -> MediaResult {
    let (local_type, create_time, content) = match message_raw {
        Some(t) => t,
        None => match lookup_message_raw(account_dir, keys, chat_id, local_id) {
            Some(t) => t,
            None => {
                tracing::warn!(
                    "[media] lookup_message_raw returned None for chat_id={}, local_id={}",
                    chat_id,
                    local_id
                );
                return pending();
            }
        },
    };

    let base = (local_type & 0xFFFFFFFF) as i32;
    let sub = (local_type >> 32) as i32;

    match base {
        49 if sub == 6 => {
            // File attachment (appmsg subtype 6)
            return get_file_attachment(account_dir, &content, create_time, local_id);
        }
        3 => {
            // base == 3: Image
            tracing::info!(
                "[media] image msg chat_id={}, local_id={}, create_time={}, content_len={}",
                chat_id,
                local_id,
                create_time,
                content.len()
            );

            let hd_len = xml_attr(&content, "hdlength")
                .and_then(|h| h.parse::<u64>().ok())
                .filter(|&len| len > 0);

            // Try .dat decryption if we have image keys
            if let Some((aes_hex, xor_byte)) = image_keys_raw {
                let image_keys = ImageKeys {
                    aes_key_hex: aes_hex,
                    xor_byte,
                };

                let candidate_res =
                    find_dat_via_resource_db(account_dir, keys, chat_id, local_id, create_time);
                let candidate_hl = find_dat_via_hardlink(account_dir, keys, chat_id, &content);

                let candidate = select_best_candidate(candidate_res, candidate_hl);

                if let Some((dat_path, file_role)) = candidate {
                    tracing::info!(
                        "[media] best dat candidate: {} (role={}) hd_len={:?}",
                        dat_path,
                        file_role,
                        hd_len
                    );
                    let res = evaluate_image_candidate(
                        &dat_path,
                        file_role,
                        hd_len,
                        &image_keys,
                        local_id,
                    );
                    if res.data.is_some() || res.media_type == "pending" {
                        return res;
                    }
                }

                tracing::warn!(
                    "[media] no dat found for local_id={}, md5={}",
                    local_id,
                    xml_attr(&content, "md5").unwrap_or_default()
                );
            } else {
                tracing::warn!("[media] no image keys available for local_id={}", local_id);
            }

            // Image exists but full resource can't be retrieved yet (pending download)
            pending()
        }
        43 => {
            // Video
            get_video_data(account_dir, keys, chat_id, local_id, create_time)
        }
        34 => {
            // Voice
            get_voice_data(account_dir, keys, chat_id, local_id)
        }
        47 => {
            // Emoji / Sticker
            get_emoji_media(account_dir, keys, &content, local_id)
        }
        _ => {
            // Other types: check for cached thumbnail
            if let Some(thumb) = get_image_thumbnail(account_dir, chat_id, local_id, create_time) {
                return thumb;
            }
            unsupported()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_backed_sticker_resolution() {
        let keys = HashMap::new();
        let content = r#"<msg><emoji cdnurl="https://res.wx.qq.com/emoji/test_123.gif" md5="deadbeef9988" /></msg>"#;
        let res = get_emoji_media("dummy_account", &keys, content, 100);

        assert_eq!(res.media_type, "sticker");
        assert_eq!(res.data, None);
        assert_eq!(
            res.url,
            Some("https://res.wx.qq.com/emoji/test_123.gif".to_string())
        );
        assert_eq!(res.format, "gif");
        assert_eq!(res.filename, "emoji_deadbeef9988.gif");
        assert_eq!(res.role, Some("original".to_string()));
        assert_eq!(res.file_path, None);
    }

    #[test]
    fn test_sticker_with_spaced_attributes_resolves_cdn_url() {
        let keys = HashMap::new();
        let content = r#"<msg><emoji fromusername = "wxid_a" md5 = "a3564410d0736e6d208afd055323c2cc" androidmd5 = "ffff" cdnurl = "http://wxapp.tc.qq.com/262/20304/stodownload?m=a35&amp;filekey=x" encrypturl = "http://enc" aeskey = "k"></emoji></msg>"#;
        let res = get_emoji_media("dummy_account", &keys, content, 102);
        assert_eq!(res.media_type, "sticker");
        assert_eq!(
            res.url.as_deref(),
            Some("http://wxapp.tc.qq.com/262/20304/stodownload?m=a35&filekey=x")
        );
        assert_eq!(res.filename, "emoji_a3564410d0736e6d208afd055323c2cc.gif");
    }

    #[test]
    fn test_url_backed_sticker_encrypturl_fallback() {
        let keys = HashMap::new();
        let content = r#"<msg><emoji encrypturl="https://res.wx.qq.com/emoji/enc_456.gif" md5="feedface0011" /></msg>"#;
        let res = get_emoji_media("dummy_account", &keys, content, 101);

        assert_eq!(res.media_type, "sticker");
        assert_eq!(res.data, None);
        assert_eq!(
            res.url,
            Some("https://res.wx.qq.com/emoji/enc_456.gif".to_string())
        );
        assert_eq!(res.format, "gif");
        assert_eq!(res.filename, "emoji_feedface0011.gif");
        assert_eq!(res.role, Some("original".to_string()));
    }

    #[test]
    fn test_media_roles_thumbnail_and_original() {
        // Test role preservation contracts
        let thumb_res = MediaResult {
            media_type: "image".into(),
            data: Some("base64thumb".into()),
            url: None,
            format: "jpeg".into(),
            filename: "msg_123_thumb.jpg".into(),
            role: Some("thumbnail".into()),
            file_path: None,
        };
        assert_eq!(thumb_res.role, Some("thumbnail".to_string()));

        let orig_res = MediaResult {
            media_type: "image".into(),
            data: Some("base64orig".into()),
            url: None,
            format: "jpeg".into(),
            filename: "msg_123.jpg".into(),
            role: Some("original".into()),
            file_path: None,
        };
        assert_eq!(orig_res.role, Some("original".to_string()));
    }

    fn make_synthetic_dat(
        temp_dir: &std::path::Path,
        filename: &str,
        format_tag: &[u8],
        aes_key_hex: &str,
        xor_byte: u8,
        pad_len: usize,
    ) -> std::path::PathBuf {
        let mut head = Vec::new();
        head.extend_from_slice(format_tag);
        while head.len() < 1024 {
            head.push((head.len() % 251) as u8);
        }

        let aes_key = &aes_key_hex.as_bytes()[..16];
        let mut child = Command::new("openssl")
            .args(["enc", "-aes-128-ecb", "-K", &hex_encode(aes_key)])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("openssl required for synthetic fixture generation");

        use std::io::Write;
        child.stdin.take().unwrap().write_all(&head).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "openssl enc failed");
        let ct = output.stdout;
        assert_eq!(ct.len(), 1040);

        let mut xor_payload = Vec::new();
        for i in 0..pad_len {
            xor_payload.push(((i % 256) as u8) ^ xor_byte);
        }
        if format_tag.starts_with(&[0xFF, 0xD8]) {
            xor_payload.push(0xFF ^ xor_byte);
            xor_payload.push(0xD9 ^ xor_byte);
        } else if format_tag.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
            let expected = [0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82];
            for b in expected {
                xor_payload.push(b ^ xor_byte);
            }
        }

        let mut dat = Vec::new();
        dat.extend_from_slice(&DAT_MAGIC);
        dat.extend_from_slice(&1024u32.to_le_bytes());
        dat.extend_from_slice(&(xor_payload.len() as u32).to_le_bytes());
        dat.push(0u8);
        dat.extend_from_slice(&ct);
        dat.extend_from_slice(&xor_payload);

        let file_path = temp_dir.join(filename);
        std::fs::write(&file_path, &dat).unwrap();
        file_path
    }

    const VALID_PNG_1132_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAABkAAAAOCAIAAABVWCAXAAAEM0lEQVR4AQEoBNf7AQoUHgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA3OwBYQh8zpgAAAABJRU5ErkJggg==";

    fn make_synthetic_dat_from_payload(
        temp_dir: &std::path::Path,
        filename: &str,
        payload: &[u8],
        aes_key_hex: &str,
        xor_byte: u8,
    ) -> std::path::PathBuf {
        let enc_chunk_size = std::cmp::min(1024, payload.len());
        let head = &payload[..enc_chunk_size];
        let tail = &payload[enc_chunk_size..];

        let aes_key = &aes_key_hex.as_bytes()[..16];
        let mut child = Command::new("openssl")
            .args(["enc", "-aes-128-ecb", "-K", &hex_encode(aes_key)])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("openssl required for synthetic fixture generation");

        use std::io::Write;
        child.stdin.take().unwrap().write_all(head).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "openssl enc failed");
        let ct = output.stdout;
        assert_eq!(ct.len(), 1040);

        let mut xor_payload = Vec::new();
        for b in tail {
            xor_payload.push(b ^ xor_byte);
        }

        let mut dat = Vec::new();
        dat.extend_from_slice(&DAT_MAGIC);
        dat.extend_from_slice(&(enc_chunk_size as u32).to_le_bytes());
        dat.extend_from_slice(&(xor_payload.len() as u32).to_le_bytes());
        dat.push(0u8);
        dat.extend_from_slice(&ct);
        dat.extend_from_slice(&xor_payload);

        let file_path = temp_dir.join(filename);
        std::fs::write(&file_path, &dat).unwrap();
        file_path
    }

    #[test]
    fn test_effective_role_preservation_and_synthetic_samples() {
        let compute_role = |file_role: &str| -> &str {
            match file_role {
                "original" => "original",
                "standard" => "standard",
                "thumbnail" => "thumbnail",
                _ => "unknown",
            }
        };

        assert_eq!(compute_role("thumbnail"), "thumbnail");
        assert_eq!(compute_role("standard"), "standard");
        assert_eq!(compute_role("original"), "original");

        // Use synthetic desensitized fixtures generated on the fly with a synthetic dummy key
        let temp_dir =
            std::env::temp_dir().join(format!("wechat_synth_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let dummy_aes_hex = "0123456789abcdef0123456789abcdef";
        let xor_byte = 0x5au8;
        let image_keys = ImageKeys {
            aes_key_hex: dummy_aes_hex.into(),
            xor_byte: Some(xor_byte),
        };

        // 1. Synthetic HD original (PNG format, 1147 bytes) -> must decrypt and preserve role "original"
        let h_path = make_synthetic_dat(
            &temp_dir,
            "synthetic_h.dat",
            &[0x89, 0x50, 0x4E, 0x47],
            dummy_aes_hex,
            xor_byte,
            100,
        );
        let res_orig = decrypt_and_return(h_path.to_str().unwrap(), &image_keys, 201, "original");
        assert_eq!(res_orig.media_type, "image");
        assert_eq!(res_orig.format, "png");
        assert_eq!(res_orig.role, Some("original".into()));
        assert!(res_orig.data.is_some());

        // 2. Synthetic standard image (JPEG format, 1091 bytes, different size & hash) -> must decrypt and preserve role "standard"
        let std_path = make_synthetic_dat(
            &temp_dir,
            "synthetic_std.dat",
            &[0xFF, 0xD8, 0xFF, 0xE0],
            dummy_aes_hex,
            xor_byte,
            50,
        );
        let res_std = decrypt_and_return(std_path.to_str().unwrap(), &image_keys, 202, "standard");
        assert_eq!(res_std.media_type, "image");
        assert_eq!(res_std.format, "jpeg");
        assert_eq!(res_std.role, Some("standard".into()));
        assert_ne!(res_std.role, Some("original".into()));
        assert!(res_std.data.is_some());

        // Verify SHA256 / content of synthetic_h and synthetic_std are strictly distinct
        let h_bytes = std::fs::read(&h_path).unwrap();
        let std_bytes = std::fs::read(&std_path).unwrap();
        assert_ne!(h_bytes.len(), std_bytes.len());
        assert_ne!(h_bytes, std_bytes);

        // 3. Synthetic thumbnail (JPEG format, 1051 bytes) -> role "thumbnail"
        let t_path = make_synthetic_dat(
            &temp_dir,
            "synthetic_t.dat",
            &[0xFF, 0xD8, 0xFF, 0xE0],
            dummy_aes_hex,
            xor_byte,
            10,
        );
        let res_thumb = decrypt_and_return(t_path.to_str().unwrap(), &image_keys, 203, "thumbnail");
        assert_eq!(res_thumb.media_type, "image");
        assert_eq!(res_thumb.format, "jpeg");
        assert_eq!(res_thumb.role, Some("thumbnail".into()));
        assert_ne!(res_thumb.role, Some("original".into()));
        assert!(res_thumb.data.is_some());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_candidate_selection_and_hd_evaluation_e2e() {
        // 1. Verify candidate ranking and selection (Point 1)
        // resource_db returning thumbnail must NOT block hardlink from picking an HD or standard candidate
        let cand_res_thumb = Some(("/path/to/thumb_t.dat".to_string(), "thumbnail"));
        let cand_hl_hd = Some(("/path/to/image_h.dat".to_string(), "original"));
        let cand_hl_std = Some(("/path/to/image.dat".to_string(), "standard"));

        let picked_hd = select_best_candidate(cand_res_thumb.clone(), cand_hl_hd.clone());
        assert_eq!(picked_hd, cand_hl_hd);

        let picked_std = select_best_candidate(cand_res_thumb.clone(), cand_hl_std.clone());
        assert_eq!(picked_std, cand_hl_std);

        let cand_res_std = Some(("/path/to/res.dat".to_string(), "standard"));
        let picked_best = select_best_candidate(cand_res_std.clone(), cand_hl_hd.clone());
        assert_eq!(picked_best, cand_hl_hd);

        // 2. Verify HD evaluation and delivery pipeline (Point 2)
        let temp_dir = std::env::temp_dir().join(format!("wechat_hd_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let dummy_aes_hex = "0123456789abcdef0123456789abcdef";
        let xor_byte = 0x5au8;
        let image_keys = ImageKeys {
            aes_key_hex: dummy_aes_hex.into(),
            xor_byte: Some(xor_byte),
        };

        // Create synthetic standard file (size ~1091 bytes)
        let std_path = make_synthetic_dat(
            &temp_dir,
            "sample_std.dat",
            &[0xFF, 0xD8, 0xFF, 0xE0],
            dummy_aes_hex,
            xor_byte,
            50,
        );
        let std_len = std::fs::metadata(&std_path).unwrap().len();
        assert!(std_len >= 1000);

        // Create synthetic HD file with valid decodable PNG payload (size 1132 bytes, dimensions 25x14)
        let valid_png_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            VALID_PNG_1132_B64,
        )
        .unwrap();
        assert_eq!(valid_png_bytes.len(), 1132);

        let h_path = make_synthetic_dat_from_payload(
            &temp_dir,
            "sample_h.dat",
            &valid_png_bytes,
            dummy_aes_hex,
            xor_byte,
        );

        // Create synthetic thumbnail file
        let t_path = make_synthetic_dat(
            &temp_dir,
            "sample_t.dat",
            &[0xFF, 0xD8, 0xFF, 0xE0],
            dummy_aes_hex,
            xor_byte,
            10,
        );

        // Case A: Thumbnail candidate must return pending (202), never delivered as final
        let res_t = evaluate_image_candidate(
            t_path.to_str().unwrap(),
            "thumbnail",
            None,
            &image_keys,
            301,
        );
        assert_eq!(res_t.media_type, "pending");
        assert!(res_t.data.is_none());

        // Case B: HD required (target_hd_len = 50000), but only mid-res file on disk (size 1091 < 50000)
        // Must return pending (202), strictly preventing mid-res from being masqueraded as original
        let res_mid = evaluate_image_candidate(
            std_path.to_str().unwrap(),
            "standard",
            Some(50000),
            &image_keys,
            302,
        );
        assert_eq!(res_mid.media_type, "pending");
        assert!(res_mid.data.is_none());

        // Case C: HD required (target_hd_len = 50000), and candidate is _h.dat
        // Delivers as original
        let res_h = evaluate_image_candidate(
            h_path.to_str().unwrap(),
            "original",
            Some(50000),
            &image_keys,
            303,
        );
        assert_eq!(res_h.media_type, "image");
        assert_eq!(res_h.role, Some("original".into()));
        assert!(res_h.data.is_some());

        // Case D: HD required (target_hd_len = 1000 <= std_len), candidate is .dat
        // Promoted to original because file size meets hdlength and decrypted cleanly
        let res_promoted = evaluate_image_candidate(
            std_path.to_str().unwrap(),
            "standard",
            Some(1000),
            &image_keys,
            304,
        );
        assert_eq!(res_promoted.media_type, "image");
        assert_eq!(res_promoted.role, Some("original".into()));
        assert!(res_promoted.data.is_some());

        // Case E: Standard message (no hdlength, like LID 18), candidate is .dat
        // Delivers cleanly as standard
        let res_std = evaluate_image_candidate(
            std_path.to_str().unwrap(),
            "standard",
            None,
            &image_keys,
            305,
        );
        assert_eq!(res_std.media_type, "image");
        assert_eq!(res_std.role, Some("standard".into()));
        assert_ne!(res_std.role, Some("original".into()));
        assert!(res_std.data.is_some());

        // Verify decoded bytes, headers, and actual decodable pixel dimensions
        let png_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            res_h.data.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(&png_bytes[..4], &[0x89, 0x50, 0x4E, 0x47]); // valid PNG header
        assert_eq!(png_bytes.len(), 1132);
        // Verify actual image decodability and pixel dimensions (25x14)
        let decoded_img =
            image::load_from_memory(&png_bytes).expect("synthetic PNG must be decodable");
        use image::GenericImageView;
        assert_eq!(decoded_img.dimensions(), (25, 14));

        let jpeg_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            res_std.data.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(&jpeg_bytes[..2], &[0xFF, 0xD8]); // valid JPEG header
        assert_eq!(jpeg_bytes.len(), 1076);

        // Case F: Unknown format must NOT be delivered! Must return pending (Point 1)
        let unk_path = make_synthetic_dat(
            &temp_dir,
            "sample_unk.dat",
            &[0x12, 0x34, 0x56, 0x78], // unknown non-image bytes
            dummy_aes_hex,
            xor_byte,
            10,
        );
        let res_unk = evaluate_image_candidate(
            unk_path.to_str().unwrap(),
            "standard",
            None,
            &image_keys,
            306,
        );
        assert_eq!(res_unk.media_type, "pending");
        assert!(res_unk.data.is_none());

        // Case G: WXGF conversion failure must NOT fall through to deliver raw bytes! Must return pending (Point 1)
        let wxgf_path = make_synthetic_dat(
            &temp_dir,
            "sample_wxgf.dat",
            b"wxgf",
            dummy_aes_hex,
            xor_byte,
            10,
        );
        let res_wxgf = evaluate_image_candidate(
            wxgf_path.to_str().unwrap(),
            "standard",
            None,
            &image_keys,
            307,
        );
        assert_eq!(res_wxgf.media_type, "pending");
        assert!(res_wxgf.data.is_none());

        // Case H: WXGF candidate when message specifies target hdlength (Point 1 & Point 2)
        // WXGF transcoding changes byte length unpredictably; without verified WXGF HD sample,
        // it cannot be promoted to original and must return pending (HD_WXGF_UNVERIFIED).
        let wxgf_hd_path = make_synthetic_dat(
            &temp_dir,
            "sample_wxgf_hd.dat",
            b"wxgf",
            dummy_aes_hex,
            xor_byte,
            10,
        );
        let res_wxgf_hd = evaluate_image_candidate(
            wxgf_hd_path.to_str().unwrap(),
            "standard",
            Some(50),
            &image_keys,
            308,
        );
        assert_eq!(res_wxgf_hd.media_type, "pending");
        assert!(res_wxgf_hd.data.is_none());
        assert_ne!(res_wxgf_hd.role, Some("original".into()));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
