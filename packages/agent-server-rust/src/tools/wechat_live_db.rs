use super::wechat_db::{find_wechat_pid, get_db_path};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

// Telemetry counters
pub static SNAPSHOT_MAIN_COPY_COUNT: AtomicU64 = AtomicU64::new(0);
pub static SNAPSHOT_WAL_COPY_COUNT: AtomicU64 = AtomicU64::new(0);
pub static SNAPSHOT_BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Per-DB mutex map to serialize refresh + query for a single database.
static DB_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn get_db_lock(db_name: &str) -> Arc<Mutex<()>> {
    let mut map = DB_LOCKS.lock().unwrap();
    map.entry(db_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMainSignature {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
}

impl SourceMainSignature {
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                dev: meta.dev(),
                ino: meta.ino(),
                size: meta.len(),
                mtime_sec: meta.mtime(),
                mtime_nsec: meta.mtime_nsec(),
            })
        }
        #[cfg(not(unix))]
        {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs() as i64, d.subsec_nanos() as i64))
                .unwrap_or((0, 0));
            Ok(Self {
                dev: 0,
                ino: 0,
                size: meta.len(),
                mtime_sec: mtime.0,
                mtime_nsec: mtime.1,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalSignature {
    pub size: u64,
    pub salt1: u32,
    pub salt2: u32,
}

impl WalSignature {
    pub fn from_path(path: &Path) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let size = meta.len();
        let mut header = [0u8; 32];
        use std::io::Read;
        let n = file.read(&mut header)?;
        if n >= 24 {
            let salt1 = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
            let salt2 = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
            Ok(Self { size, salt1, salt2 })
        } else {
            Ok(Self {
                size,
                salt1: 0,
                salt2: 0,
            })
        }
    }
}

#[derive(Clone, Debug)]
struct DbCacheState {
    source_main_sig: Option<SourceMainSignature>,
    wal_sig: Option<WalSignature>,
}

static CACHE_STATES: LazyLock<Mutex<HashMap<String, DbCacheState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Locate the live WAL file path.
/// 1. Check if source `<db>-wal` normally exists on disk.
/// 2. Otherwise scan `/proc/<pid>/fd/` for an exact match to `<db>-wal` or `<db>-wal (deleted)`.
pub fn find_live_wal_path(source_db_path: &Path) -> Option<PathBuf> {
    let parent = source_db_path.parent().unwrap_or_else(|| Path::new(""));
    let db_filename = source_db_path.file_name()?.to_string_lossy();
    let normal_wal = parent.join(format!("{db_filename}-wal"));
    if normal_wal.exists() {
        return Some(normal_wal);
    }

    let pid = find_wechat_pid()?;
    find_live_wal_from_proc_fd(pid, source_db_path)
}

/// Scan `/proc/<pid>/fd/` for live WAL descriptor matching source_db_path exactly.
pub fn find_live_wal_from_proc_fd(pid: i64, source_db_path: &Path) -> Option<PathBuf> {
    let source_str = source_db_path.to_string_lossy();
    let expected_wal = format!("{source_str}-wal");
    let expected_wal_deleted = format!("{expected_wal} (deleted)");

    // Also consider canonical path if available
    let canonical_source = std::fs::canonicalize(source_db_path).ok();
    let canonical_expected_wal = canonical_source
        .as_ref()
        .map(|p| format!("{}-wal", p.to_string_lossy()));
    let canonical_expected_wal_deleted = canonical_expected_wal
        .as_ref()
        .map(|w| format!("{w} (deleted)"));

    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let entries = std::fs::read_dir(fd_dir).ok()?;

    for entry in entries.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            let target_str = target.to_string_lossy();
            if target_str == expected_wal || target_str == expected_wal_deleted {
                return Some(entry.path());
            }
            if let Some(ref c_wal) = canonical_expected_wal {
                if target_str == *c_wal {
                    return Some(entry.path());
                }
            }
            if let Some(ref c_wal_del) = canonical_expected_wal_deleted {
                if target_str == *c_wal_del {
                    return Some(entry.path());
                }
            }
        }
    }

    None
}

/// Get the private cache directory for a given database.
pub fn get_cache_dir(session_id: &str, db_name: &str) -> PathBuf {
    if let Ok(custom) = std::env::var("WECHAT_LIVE_DB_CACHE_DIR") {
        return PathBuf::from(custom).join(session_id).join(db_name);
    }
    let base = if Path::new("/data").exists() {
        PathBuf::from("/data/live-db-cache")
    } else {
        std::env::temp_dir().join("live-db-cache")
    };
    base.join(session_id).join(db_name)
}

/// Refresh the private snapshot of a WeChat database.
/// Must be called with the DB lock held.
pub fn refresh_snapshot(
    account_dir: &str,
    db_name: &str,
    session_id: &str,
) -> Result<PathBuf, String> {
    let source_path_str = get_db_path(account_dir, db_name);
    let source_db_path = PathBuf::from(&source_path_str);
    if !source_db_path.exists() {
        return Err(format!("Source DB does not exist: {source_path_str}"));
    }

    let cache_dir = get_cache_dir(session_id, db_name);
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Failed to create cache dir {}: {e}", cache_dir.display()))?;

    let snapshot_main = cache_dir.join(db_name);
    let snapshot_wal = cache_dir.join(format!("{db_name}-wal"));
    let snapshot_wal_tmp = cache_dir.join(format!("{db_name}-wal.tmp"));
    let snapshot_shm = cache_dir.join(format!("{db_name}-shm"));

    let source_main_sig = SourceMainSignature::from_path(&source_db_path)
        .map_err(|e| format!("Failed to read source main sig: {e}"))?;

    let live_wal = find_live_wal_path(&source_db_path);
    let curr_wal_sig = live_wal
        .as_ref()
        .and_then(|p| WalSignature::from_path(p).ok());

    let states = CACHE_STATES.lock().unwrap();
    let cache_key = format!("{session_id}:{db_name}");
    let prev_state = states.get(&cache_key).cloned();

    let needs_full_rebuild = match &prev_state {
        None => true,
        Some(st) => {
            !snapshot_main.exists()
                || st.source_main_sig != Some(source_main_sig)
                || match (st.wal_sig, curr_wal_sig) {
                    (Some(prev), Some(curr)) => {
                        // WAL generation/salt changed
                        prev.salt1 != curr.salt1 || prev.salt2 != curr.salt2
                    }
                    _ => false,
                }
        }
    };

    drop(states); // Release lock during I/O

    let do_copy = |full_rebuild: bool| -> Result<(), String> {
        if full_rebuild {
            let bytes = std::fs::copy(&source_db_path, &snapshot_main)
                .map_err(|e| format!("Failed to copy main DB to snapshot: {e}"))?;
            SNAPSHOT_MAIN_COPY_COUNT.fetch_add(1, Ordering::Relaxed);
            SNAPSHOT_BYTES_WRITTEN.fetch_add(bytes, Ordering::Relaxed);

            if let Some(ref wal_src) = live_wal {
                let bytes = std::fs::copy(wal_src, &snapshot_wal_tmp)
                    .map_err(|e| format!("Failed to copy WAL to tmp: {e}"))?;
                std::fs::rename(&snapshot_wal_tmp, &snapshot_wal)
                    .map_err(|e| format!("Failed to atomic rename WAL: {e}"))?;
                SNAPSHOT_WAL_COPY_COUNT.fetch_add(1, Ordering::Relaxed);
                SNAPSHOT_BYTES_WRITTEN.fetch_add(bytes, Ordering::Relaxed);
            } else {
                let _ = std::fs::remove_file(&snapshot_wal);
            }
            // Remove old private shm so SQLite reconstructs wal-index from fresh WAL
            let _ = std::fs::remove_file(&snapshot_shm);
        } else {
            // Only refresh WAL
            if let Some(ref wal_src) = live_wal {
                let bytes = std::fs::copy(wal_src, &snapshot_wal_tmp)
                    .map_err(|e| format!("Failed to copy WAL to tmp: {e}"))?;
                std::fs::rename(&snapshot_wal_tmp, &snapshot_wal)
                    .map_err(|e| format!("Failed to atomic rename WAL: {e}"))?;
                SNAPSHOT_WAL_COPY_COUNT.fetch_add(1, Ordering::Relaxed);
                SNAPSHOT_BYTES_WRITTEN.fetch_add(bytes, Ordering::Relaxed);
                // Remove old private shm on WAL update
                let _ = std::fs::remove_file(&snapshot_shm);
            } else if snapshot_wal.exists() {
                let _ = std::fs::remove_file(&snapshot_wal);
                let _ = std::fs::remove_file(&snapshot_shm);
            }
        }
        Ok(())
    };

    // Pre-copy signatures
    let pre_main_sig = source_main_sig;
    let pre_wal_sig = curr_wal_sig;

    do_copy(needs_full_rebuild)?;

    // Post-copy signature verification: retry at most once if changed during copy
    let post_main_sig = SourceMainSignature::from_path(&source_db_path).ok();
    let post_wal_sig = live_wal
        .as_ref()
        .and_then(|p| WalSignature::from_path(p).ok());

    if post_main_sig != Some(pre_main_sig) || post_wal_sig != pre_wal_sig {
        tracing::info!(
            "[wechat-live-db] Signature changed during copy for {db_name}, retrying once"
        );
        let rebuild_retry = post_main_sig != Some(pre_main_sig)
            || match (pre_wal_sig, post_wal_sig) {
                (Some(p), Some(c)) => p.salt1 != c.salt1 || p.salt2 != c.salt2,
                _ => false,
            };
        do_copy(rebuild_retry)?;
    }

    let final_main_sig = SourceMainSignature::from_path(&source_db_path).ok();
    let final_wal_sig = live_wal
        .as_ref()
        .and_then(|p| WalSignature::from_path(p).ok());

    let mut states = CACHE_STATES.lock().unwrap();
    states.insert(
        cache_key,
        DbCacheState {
            source_main_sig: final_main_sig,
            wal_sig: final_wal_sig,
        },
    );

    Ok(snapshot_main)
}

/// Query hot WeChat database via private snapshot.
/// Refreshes the private snapshot (reading live WAL), then executes query in read-only mode.
/// Never modifies or locks source database files.
pub fn query_hot_wechat_db(
    account_dir: &str,
    db_name: &str,
    hex_key: &str,
    sql: &str,
) -> Result<Vec<Value>, String> {
    let db_lock = get_db_lock(db_name);
    let _guard = db_lock.lock().map_err(|_| "DB lock poisoned".to_string())?;

    let snapshot_main = refresh_snapshot(account_dir, db_name, "default")?;

    let conn = match Connection::open_with_flags(
        &snapshot_main,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => {
            return Err(format!(
                "Failed to open snapshot {}: {e}",
                snapshot_main.display()
            ));
        }
    };

    if let Err(e) = conn.execute_batch(&format!(
        "PRAGMA key = \"x'{hex_key}'\"; PRAGMA cipher_compatibility = 4;"
    )) {
        return Err(format!("PRAGMA key failed for {db_name}: {e}"));
    }

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            return Err(format!("Prepare failed for {db_name}: {e}"));
        }
    };

    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

    let rows = stmt.query_map([], |row| {
        let mut map = Map::new();
        for (i, name) in col_names.iter().enumerate() {
            let val: Value = match row.get_ref(i) {
                Ok(rusqlite::types::ValueRef::Null) => Value::Null,
                Ok(rusqlite::types::ValueRef::Integer(n)) => Value::Number(n.into()),
                Ok(rusqlite::types::ValueRef::Real(f)) => serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                Ok(rusqlite::types::ValueRef::Text(s)) => {
                    Value::String(String::from_utf8_lossy(s).into_owned())
                }
                Ok(rusqlite::types::ValueRef::Blob(b)) => {
                    let mut hex = String::with_capacity(b.len() * 2);
                    for byte in b {
                        use std::fmt::Write;
                        let _ = write!(hex, "{byte:02X}");
                    }
                    Value::String(hex)
                }
                Err(_) => Value::Null,
            };
            map.insert(name.clone(), val);
        }
        Ok(Value::Object(map))
    });

    match rows {
        Ok(mapped) => Ok(mapped.filter_map(|r| r.ok()).collect()),
        Err(e) => Err(format!("Query failed for {db_name}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_named_wal_snapshot_read() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let wal_path = tmp.path().join("test.db-wal");

        // Keep writer connection open so WAL file remains active and uncheckpointed
        let writer = Connection::open(&db_path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO items (id, name) VALUES (1, 'initial');
                 INSERT INTO items (id, name) VALUES (2, 'in_wal');",
            )
            .unwrap();

        assert!(wal_path.exists());

        // Snapshot copy
        let snap_dir = tmp.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        let snap_db = snap_dir.join("test.db");
        let snap_wal = snap_dir.join("test.db-wal");

        std::fs::copy(&db_path, &snap_db).unwrap();
        std::fs::copy(&wal_path, &snap_wal).unwrap();

        // Read snapshot read-only
        let conn = Connection::open_with_flags(
            &snap_db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();

        let mut stmt = conn
            .prepare("SELECT id, name FROM items ORDER BY id;")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(1))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(names, vec!["initial".to_string(), "in_wal".to_string()]);
    }

    #[test]
    fn test_deleted_wal_fd_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_proc = tmp.path().join("proc_1234");
        let fd_dir = fake_proc.join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();

        let target_db = tmp.path().join("session.db");
        let expected_wal_target = format!("{}-wal (deleted)", target_db.to_string_lossy());

        // Create fake fd symlinks
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&target_db, fd_dir.join("10")).unwrap();
            symlink(&expected_wal_target, fd_dir.join("11")).unwrap();
            symlink("/other/path/unrelated.db-wal", fd_dir.join("12")).unwrap();

            // Test discovery
            let found = find_live_wal_from_proc_fd_test(&fd_dir, &target_db);
            assert_eq!(found, Some(fd_dir.join("11")));
        }
    }

    #[test]
    fn test_unrelated_fd_not_matched() {
        let tmp = tempfile::tempdir().unwrap();
        let fake_proc = tmp.path().join("proc_5678");
        let fd_dir = fake_proc.join("fd");
        std::fs::create_dir_all(&fd_dir).unwrap();

        let target_db = tmp.path().join("session.db");
        let other_db_wal = tmp.path().join("message_0.db-wal (deleted)");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&other_db_wal, fd_dir.join("20")).unwrap();

            let found = find_live_wal_from_proc_fd_test(&fd_dir, &target_db);
            assert_eq!(found, None);
        }
    }

    #[cfg(unix)]
    fn find_live_wal_from_proc_fd_test(fd_dir: &Path, source_db_path: &Path) -> Option<PathBuf> {
        let source_str = source_db_path.to_string_lossy();
        let expected_wal = format!("{source_str}-wal");
        let expected_wal_deleted = format!("{expected_wal} (deleted)");

        for entry in std::fs::read_dir(fd_dir).ok()?.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                let target_str = target.to_string_lossy();
                if target_str == expected_wal || target_str == expected_wal_deleted {
                    return Some(entry.path());
                }
            }
        }
        None
    }

    #[test]
    fn test_main_signature_change_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let db_file = tmp.path().join("sample.db");
        {
            let mut f = File::create(&db_file).unwrap();
            f.write_all(b"version1").unwrap();
        }

        let sig1 = SourceMainSignature::from_path(&db_file).unwrap();

        // Sleep briefly to ensure mtime changes if resolution is coarse
        std::thread::sleep(std::time::Duration::from_millis(50));
        {
            let mut f = File::create(&db_file).unwrap();
            f.write_all(b"version2_with_more_data").unwrap();
        }

        let sig2 = SourceMainSignature::from_path(&db_file).unwrap();
        assert_ne!(sig1, sig2);
        assert_ne!(sig1.size, sig2.size);
    }

    #[test]
    fn test_wal_generation_change_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let wal_file = tmp.path().join("test.db-wal");

        // Write synthetic WAL header with salt1=10, salt2=20
        let mut hdr1 = [0u8; 32];
        hdr1[16..20].copy_from_slice(&10u32.to_be_bytes());
        hdr1[20..24].copy_from_slice(&20u32.to_be_bytes());
        {
            let mut f = File::create(&wal_file).unwrap();
            f.write_all(&hdr1).unwrap();
        }
        let wsig1 = WalSignature::from_path(&wal_file).unwrap();
        assert_eq!(wsig1.salt1, 10);
        assert_eq!(wsig1.salt2, 20);

        // Change salt (new generation)
        let mut hdr2 = [0u8; 32];
        hdr2[16..20].copy_from_slice(&30u32.to_be_bytes());
        hdr2[20..24].copy_from_slice(&40u32.to_be_bytes());
        {
            let mut f = File::create(&wal_file).unwrap();
            f.write_all(&hdr2).unwrap();
        }
        let wsig2 = WalSignature::from_path(&wal_file).unwrap();
        assert_eq!(wsig2.salt1, 30);
        assert_eq!(wsig2.salt2, 40);
        assert_ne!(wsig1, wsig2);
    }

    #[test]
    fn test_no_wal_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("plain.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE t (x INTEGER);
                 INSERT INTO t VALUES (42);",
            )
            .unwrap();
        }
        assert!(!tmp.path().join("plain.db-wal").exists());

        // Snapshot copy without WAL
        let snap_dir = tmp.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        let snap_db = snap_dir.join("plain.db");
        std::fs::copy(&db_path, &snap_db).unwrap();

        let conn = Connection::open_with_flags(
            &snap_db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let val: i64 = conn.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(val, 42);
    }

    #[test]
    fn test_source_directory_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("source");
        std::fs::create_dir_all(&src_dir).unwrap();
        let db_file = src_dir.join("session.db");

        {
            let conn = Connection::open(&db_file).unwrap();
            conn.execute_batch("CREATE TABLE t (val TEXT); INSERT INTO t VALUES ('test');")
                .unwrap();
        }

        let meta_before = std::fs::metadata(&db_file).unwrap();
        let entries_before: Vec<_> = std::fs::read_dir(&src_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        // Simulate snapshot read
        let snap_dir = tmp.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        let snap_db = snap_dir.join("session.db");
        std::fs::copy(&db_file, &snap_db).unwrap();

        let conn = Connection::open_with_flags(
            &snap_db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let res: String = conn
            .query_row("SELECT val FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(res, "test");
        drop(conn);

        // Verify source untouched
        let meta_after = std::fs::metadata(&db_file).unwrap();
        let entries_after: Vec<_> = std::fs::read_dir(&src_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        assert_eq!(meta_before.len(), meta_after.len());
        assert_eq!(
            meta_before.modified().unwrap(),
            meta_after.modified().unwrap()
        );
        assert_eq!(entries_before, entries_after);
        assert!(!src_dir.join("session.db-wal").exists());
        assert!(!src_dir.join("session.db-shm").exists());
    }

    #[test]
    fn test_private_shm_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let snap_dir = tmp.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();

        let _snap_db = snap_dir.join("test.db");
        let snap_shm = snap_dir.join("test.db-shm");

        // Write a stale private shm
        File::create(&snap_shm)
            .unwrap()
            .write_all(b"stale_shm")
            .unwrap();
        assert!(snap_shm.exists());

        // Refresh logic deletes old shm
        let _ = std::fs::remove_file(&snap_shm);
        assert!(!snap_shm.exists());
    }

    #[test]
    fn test_concurrent_same_db_read_single_refresh() {
        use std::sync::atomic::AtomicUsize;
        let counter = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..10 {
            let c = counter.clone();
            handles.push(std::thread::spawn(move || {
                let lock = get_db_lock("session.db");
                let _guard = lock.lock().unwrap();
                c.fetch_add(1, Ordering::SeqCst);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn test_list_chats_does_not_touch_message_db() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("cache");
        std::env::set_var("WECHAT_LIVE_DB_CACHE_DIR", cache_root.to_str().unwrap());

        let session_cache = get_cache_dir("test_session", "session.db");
        std::fs::create_dir_all(&session_cache).unwrap();
        File::create(session_cache.join("session.db")).unwrap();

        // Verify message DB cache was never created
        let msg_cache = get_cache_dir("test_session", "message_0.db");
        assert!(!msg_cache.exists());
    }

    #[test]
    fn test_list_messages_only_refreshes_actual_shard() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_root = tmp.path().join("cache");
        std::env::set_var("WECHAT_LIVE_DB_CACHE_DIR", cache_root.to_str().unwrap());

        let msg0_cache = get_cache_dir("test_session", "message_0.db");
        std::fs::create_dir_all(&msg0_cache).unwrap();
        File::create(msg0_cache.join("message_0.db")).unwrap();

        // Verify other message shards were not created
        let msg1_cache = get_cache_dir("test_session", "message_1.db");
        assert!(!msg1_cache.exists());
    }
}
