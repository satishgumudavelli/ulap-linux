//! Scan folders, upload pending files to Telegram, publish the pinned backup index, restore.
use crate::config::{data_dir, Config};
use crate::store::{Item, Store};
use crate::telegram::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const INDEX_CAPTION: &str = "[ulap-backup-index]";
const INDEX_FILENAME: &str = "ulap_index_latest.json";
const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "heic"];
const VIDEO_EXT: &[&str] = &["mp4", "mkv", "mov", "webm", "avi", "3gp", "m4v"];

#[derive(Default)]
pub struct Progress {
    pub running: bool,
    pub task: String,
    pub done: usize,
    pub total: usize,
    pub current: String,
    pub log: Vec<String>,
    /// bumped when items change so the UI reloads
    pub version: u64,
}

pub type Shared = Arc<Mutex<Progress>>;

fn say(p: &Shared, s: impl Into<String>) {
    let mut g = p.lock().unwrap();
    g.log.push(s.into());
    if g.log.len() > 300 {
        g.log.remove(0);
    }
}

// Same JSON as the Android app's schema v2 so backups are readable on both.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    id: String,
    telegram_file_id: String,
    telegram_message_id: Option<i64>,
    file_name: String,
    mime_type: String,
    size: i64,
    date_taken: i64,
    bucket_name: String,
    media_type: String,
    duration_ms: Option<i64>,
    thumbnail_file_id: Option<String>,
    chunk_message_ids: Option<Vec<i64>>,
    chunk_count: Option<i32>,
    chunk_file_ids: Option<Vec<String>>,
    chunk_byte_lengths: Option<Vec<i64>>,
    upload_bot_index: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: i32,
    exported_at: i64,
    items: Vec<IndexEntry>,
    /// PDFs, ebooks and other non-media files. The Android app ignores unknown keys, and its
    /// `mediaType` only accepts IMAGE/VIDEO, so these must not be mixed into `items`.
    #[serde(default)]
    files: Vec<IndexEntry>,
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// `all_types`: take any file (dropped by the user); otherwise only photos and videos.
pub fn scan(cfg: &Config, store: &Store, p: &Shared, all_types: bool) -> usize {
    let mut added = 0;
    for folder in &cfg.folders {
        for e in walkdir::WalkDir::new(folder).into_iter().flatten() {
            if !e.file_type().is_file() {
                continue;
            }
            let path = e.path();
            let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
            let video = VIDEO_EXT.contains(&ext.as_str());
            let image = IMAGE_EXT.contains(&ext.as_str());
            if !video && !image && !all_types {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            if md.len() == 0 {
                continue;
            }
            let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64).unwrap_or(0);
            let ps = path.to_string_lossy().to_string();
            let fname: String = path.file_name().unwrap().to_string_lossy().into();
            if store.has_remote(&fname, md.len() as i64) {
                continue; // already backed up (e.g. restored from Telegram)
            }
            let item = Item {
                id: fnv(&format!("{}|{ps}|{}|{mtime}", cfg.chat_id, md.len())),
                name: path.file_name().unwrap().to_string_lossy().into(),
                mime: mime_guess::from_path(path).first_or_octet_stream().to_string(),
                size: md.len() as i64,
                mtime,
                bucket: path.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into()).unwrap_or_default(),
                video,
                other: !video && !image,
                status: "PENDING".into(),
                path: ps,
                file_id: None, message_id: None, bot: 0, chunk_ids: vec![], chunk_lens: vec![],
                chunk_msgs: vec![], thumb: None, error: None, thumb_file_id: None,
            };
            if !store.insert_if_new(&item) {
                store.revive(&item.id);
            } else {
                added += 1;
                if !item.video && !item.other {
                    make_thumb(&item, store);
                }
            }
        }
    }
    say(p, format!("Scan: {added} new file(s)"));
    added
}

fn make_thumb(i: &Item, store: &Store) {
    let dir = data_dir().join("thumbs");
    std::fs::create_dir_all(&dir).ok();
    let out = dir.join(format!("{}.jpg", i.id));
    if let Ok(img) = image::open(&i.path) {
        if img.thumbnail(256, 256).to_rgb8().save(&out).is_ok() {
            store.set_thumb(&i.id, &out.to_string_lossy());
        }
    }
}

fn read_exact_or_eof(f: &mut dyn Read, n: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    (&mut *f).take(n).read_to_end(&mut buf)?;
    Ok(buf)
}

fn upload_one(tg: &Tg, i: &mut Item, bot: usize, p: &Shared, strip_gps: bool) -> Result<(), String> {
    let mut file = std::fs::File::open(&i.path).map_err(|e| format!("Could not open file: {e}"))?;
    let mut size = file.metadata().map_err(|e| e.to_string())?.len();
    if size == 0 {
        return Err("Empty file".into());
    }
    let mut src: Box<dyn Read> = if strip_gps && !i.video && !i.other && i.mime == "image/jpeg" {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        // never upload a photo whose location we could not remove
        match crate::exif::strip_gps(&buf).map_err(|e| format!("Could not strip location ({e}); turn the option off to upload it as is"))? {
            Some(clean) => {
                size = clean.len() as u64;
                i.size = size as i64; // restore checks the size of what Telegram holds
                say(p, format!("  {}: location removed", i.name));
                Box::new(std::io::Cursor::new(clean))
            }
            None => Box::new(std::io::Cursor::new(buf)),
        }
    } else {
        Box::new(file)
    };
    let f = &mut *src;
    i.bot = bot as i64;
    // Other files (PDF, ebooks, ...) go up whole when Telegram allows it, so they open normally in
    // Telegram. Media over 20 MB still splits: the bot could not download it back otherwise.
    if size <= MAX_SINGLE || (i.other && size <= BOT_UPLOAD_MAX) {
        let data = read_exact_or_eof(f, size).map_err(|e| e.to_string())?;
        let d = tg.send_document(bot, &i.name, data, "")?;
        i.file_id = Some(d.file_id);
        i.message_id = Some(d.message_id);
        return Ok(());
    }
    let lens = chunk_lengths(size);
    let n = lens.len();
    i.chunk_ids.clear();
    i.chunk_lens.clear();
    i.chunk_msgs.clear();
    for (idx, len) in lens.iter().enumerate() {
        say(p, format!("  {} part {}/{}", i.name, idx + 1, n));
        let data = read_exact_or_eof(f, *len).map_err(|e| e.to_string())?;
        if data.len() as u64 != *len {
            return Err("File changed during upload".into());
        }
        let d = tg.send_document(bot, &format!("{}.part{}", i.name, idx + 1), data,
            &format!("[ulap-chunk] {} part {}/{}", i.name, idx + 1, n))?;
        i.chunk_ids.push(d.file_id);
        i.chunk_lens.push(*len as i64);
        i.chunk_msgs.push(d.message_id);
    }
    i.file_id = Some(format!("{CHUNKED_PREFIX}{n}"));
    i.message_id = Some(0);
    Ok(())
}

fn to_entry(i: &Item) -> IndexEntry {
    let chunked = !i.chunk_ids.is_empty();
    IndexEntry {
        id: i.id.clone(),
        telegram_file_id: i.file_id.clone().unwrap_or_default(),
        telegram_message_id: i.message_id,
        file_name: i.name.clone(),
        mime_type: i.mime.clone(),
        size: i.size,
        date_taken: i.mtime,
        bucket_name: i.bucket.clone(),
        media_type: if i.other { "FILE" } else if i.video { "VIDEO" } else { "IMAGE" }.into(),
        chunk_message_ids: chunked.then(|| i.chunk_msgs.clone()),
        chunk_count: chunked.then(|| i.chunk_ids.len() as i32),
        chunk_file_ids: chunked.then(|| i.chunk_ids.clone()),
        chunk_byte_lengths: chunked.then(|| i.chunk_lens.clone()),
        upload_bot_index: Some(i.bot),
        ..Default::default()
    }
}

/// (media items, other files) from the pinned index.
fn fetch_index(tg: &Tg) -> Result<(Vec<IndexEntry>, Vec<IndexEntry>), String> {
    let Some((fid, cap)) = tg.pinned_document(0)? else { return Ok((vec![], vec![])) };
    if cap != INDEX_CAPTION {
        return Ok((vec![], vec![]));
    }
    let bytes = tg.download(0, &fid)?;
    let m = serde_json::from_slice::<Manifest>(&bytes).map_err(|e| format!("bad index: {e}"))?;
    Ok((m.items, m.files))
}

/// Merge local backed-up items into the pinned index, upload and pin it.
fn export_index(tg: &Tg, store: &Store, exclude: &HashSet<String>) -> Result<(), String> {
    let local: Vec<Item> = store.all().into_iter().filter(|i| i.status == "BACKED_UP").collect();
    let mut items: Vec<IndexEntry> = local.iter().filter(|i| !i.other).map(to_entry).collect();
    let mut files: Vec<IndexEntry> = local.iter().filter(|i| i.other).map(to_entry).collect();
    items.retain(|e| !exclude.contains(&e.telegram_file_id));
    files.retain(|e| !exclude.contains(&e.telegram_file_id));
    let have: HashSet<String> = items.iter().chain(files.iter()).map(|e| e.telegram_file_id.clone()).collect();
    // keep entries other devices already published (minus what was just deleted)
    let (r_items, r_files) = fetch_index(tg)?;
    let keep = |e: &IndexEntry| !have.contains(&e.telegram_file_id) && !exclude.contains(&e.telegram_file_id);
    items.extend(r_items.into_iter().filter(|e| keep(e)));
    files.extend(r_files.into_iter().filter(|e| keep(e)));
    if items.is_empty() && files.is_empty() && exclude.is_empty() {
        return Ok(());
    }
    let m = Manifest { schema_version: 2, exported_at: now_ms(), items, files };
    let d = tg.send_document(0, INDEX_FILENAME, serde_json::to_vec(&m).unwrap(), INDEX_CAPTION)?;
    tg.pin(0, d.message_id)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Errors that retrying cannot fix.
fn permanent(e: &str) -> bool {
    e.contains("Telegram 401") || e.contains("Telegram 403") || e.starts_with("Empty file") || e.starts_with("Could not open")
}

fn logged(cfg: &Config, p: &Shared) -> Tg {
    let p2 = p.clone();
    Tg::new(&cfg.tokens, &cfg.chat_id).with_logger(Arc::new(move |s| say(&p2, s)))
}

/// Sleep `secs`, returning early (true) on cancel.
fn wait_cancelable(secs: u64, cancel: &AtomicBool) -> bool {
    for _ in 0..secs {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    cancel.load(Ordering::Relaxed)
}

const RETRY_ROUNDS: u32 = 3;
const RETRY_PAUSE_SECS: u64 = 30;

/// `only`: upload just what lies under `cfg.folders` (files dropped on the window), not other pending items.
pub fn run_sync(cfg: Config, store: Store, p: Shared, cancel: Arc<AtomicBool>, only: bool) {
    begin(&p, if only { "Uploading dropped files" } else { "Syncing" });
    let roots: Vec<String> = cfg.folders.clone();
    let pend = |store: &Store| -> Vec<Item> {
        store.pending().into_iter().filter(|i| !only || roots.iter().any(|r| Path::new(&i.path).starts_with(r))).collect()
    };
    scan(&cfg, &store, &p, only);
    p.lock().unwrap().version += 1;
    let tg = logged(&cfg, &p);
    let (mut ok_total, mut failed_total) = (0, 0);
    let mut round = 0;
    loop {
        let todo: Vec<Item> = pend(&store)
            .into_iter()
            .filter(|i| round == 0 || !i.error.as_deref().map_or(false, permanent))
            .collect();
        if todo.is_empty() {
            break;
        }
        {
            let mut g = p.lock().unwrap();
            g.total = todo.len();
            g.done = 0;
        }
        let (mut ok, mut failed) = (0, 0);
        for (n, mut item) in todo.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                say(&p, "Cancelled");
                break;
            }
            p.lock().unwrap().current = item.name.clone();
            let bot = tg.pick_bot(n); // round-robin, skips rate-limited bots
            match upload_one(&tg, &mut item, bot, &p, cfg.strip_gps) {
                Ok(()) => {
                    store.set_done(&item);
                    ok += 1;
                }
                Err(e) => {
                    say(&p, format!("FAILED {}: {e}", item.name));
                    store.set_failed(&item.id, &e);
                    failed += 1;
                }
            }
            {
                let mut g = p.lock().unwrap();
                g.done = n + 1;
                g.version += 1;
            }
            if cfg.conservative && wait_cancelable(3, &cancel) {
                break;
            }
        }
        ok_total += ok;
        failed_total = failed;
        let retryable = pend(&store).iter().any(|i| !i.error.as_deref().map_or(false, permanent));
        if cancel.load(Ordering::Relaxed) || failed == 0 || !retryable || round >= RETRY_ROUNDS || (round > 0 && ok == 0) {
            break;
        }
        round += 1;
        say(&p, format!("{failed} file(s) failed - retrying in {RETRY_PAUSE_SECS}s (round {round}/{RETRY_ROUNDS})"));
        if wait_cancelable(RETRY_PAUSE_SECS, &cancel) {
            break;
        }
    }
    if ok_total > 0 {
        match export_index(&tg, &store, &HashSet::new()) {
            Ok(()) => say(&p, "Backup index updated"),
            Err(e) => say(&p, format!("Index upload failed: {e}")),
        }
    }
    say(&p, format!("Done: {ok_total} uploaded, {failed_total} still failing"));
    end(&p);
}

fn entry_to_item(e: &IndexEntry, chat: &str, other: bool) -> Item {
    Item {
        id: format!("{chat}:{}", e.id), // the same Android id may exist in several chats
        path: String::new(),
        name: e.file_name.clone(),
        mime: e.mime_type.clone(),
        size: e.size,
        mtime: e.date_taken,
        bucket: e.bucket_name.clone(),
        video: e.media_type == "VIDEO",
        other,
        status: "CLOUD_ONLY".into(),
        file_id: Some(e.telegram_file_id.clone()),
        message_id: e.telegram_message_id,
        bot: e.upload_bot_index.unwrap_or(0),
        chunk_ids: e.chunk_file_ids.clone().unwrap_or_default(),
        chunk_lens: e.chunk_byte_lengths.clone().unwrap_or_default(),
        chunk_msgs: e.chunk_message_ids.clone().unwrap_or_default(),
        thumb: None,
        error: None,
        thumb_file_id: e.thumbnail_file_id.clone(),
    }
}

/// Read the pinned index from Telegram and add what is there to the gallery.
pub fn run_refresh(cfg: Config, store: Store, p: Shared) {
    begin(&p, "Loading gallery from Telegram");
    let tg = logged(&cfg, &p);
    match fetch_index(&tg) {
        Ok((entries, files)) => {
            let mut items: Vec<Item> = entries.iter().map(|e| entry_to_item(e, &cfg.chat_id, false)).collect();
            items.extend(files.iter().map(|e| entry_to_item(e, &cfg.chat_id, true)));
            store.upsert_remote_batch(&items);
            say(&p, format!("Telegram gallery: {} item(s) and {} file(s) in the index", entries.len(), files.len()));
        }
        Err(e) => say(&p, format!("Could not read Telegram index: {e}")),
    }
    end(&p);
}

/// Fetch a thumbnail for a cloud item: its Telegram thumbnail, else the file itself when small.
pub fn fetch_thumb(tg: &Tg, i: &Item, store: &Store) {
    if i.other || (i.video && i.thumb_file_id.is_none()) {
        return;
    }
    let bot = (i.bot.max(0) as usize) % tg.bots();
    let bytes = match &i.thumb_file_id {
        Some(t) => tg.download(bot, t),
        None if i.chunk_ids.is_empty() && i.size <= 8 * 1024 * 1024 => match &i.file_id {
            Some(f) => tg.download(bot, f),
            None => return,
        },
        None => return,
    };
    let Ok(bytes) = bytes else { return };
    let Ok(img) = image::load_from_memory(&bytes) else { return };
    let dir = data_dir().join("thumbs");
    std::fs::create_dir_all(&dir).ok();
    let out = dir.join(format!("{}.jpg", fnv(&i.id)));
    if img.thumbnail(256, 256).to_rgb8().save(&out).is_ok() {
        store.set_thumb(&i.id, &out.to_string_lossy());
    }
}

/// Delete items from Telegram (their messages) and from the pinned index. Local files are never touched.
pub fn run_delete(cfg: Config, store: Store, p: Shared, items: Vec<Item>) {
    begin(&p, "Deleting from Telegram");
    let tg = logged(&cfg, &p);
    p.lock().unwrap().total = items.len();
    let mut removed: HashSet<String> = HashSet::new();
    let mut gone: Vec<&Item> = vec![];
    for (n, i) in items.iter().enumerate() {
        p.lock().unwrap().current = i.name.clone();
        let mut ids: Vec<i64> = i.message_id.into_iter().filter(|m| *m > 0).collect();
        ids.extend(i.chunk_msgs.iter().copied().filter(|m| *m > 0));
        match tg.delete_messages(i.bot.max(0) as usize % tg.bots(), &ids) {
            Ok(()) => {
                removed.extend(i.file_id.clone());
                gone.push(i);
            }
            Err(e) => say(&p, format!("FAILED to delete {}: {e}", i.name)),
        }
        p.lock().unwrap().done = n + 1;
    }
    for i in &gone {
        if i.path.is_empty() {
            store.delete_ids(&[i.id.clone()]); // cloud only: nothing left
        } else {
            store.exclude(&i.id); // still on this PC
        }
        if let Some(t) = &i.thumb {
            if i.path.is_empty() {
                std::fs::remove_file(t).ok();
            }
        }
    }
    if !gone.is_empty() {
        match export_index(&tg, &store, &removed) {
            Ok(()) => say(&p, "Backup index updated"),
            Err(e) => say(&p, format!("Deleted, but index update failed (press Refresh to retry later): {e}")),
        }
    }
    say(&p, format!("Deleted {} of {} item(s) from Telegram", gone.len(), items.len()));
    end(&p);
}

/// Download a cloud item to the cache and return its path (for opening).
pub fn download_for_open(tg: &Tg, i: &Item) -> Result<std::path::PathBuf, String> {
    let name = Path::new(&i.name).file_name().ok_or("bad name")?.to_owned();
    let dest = data_dir().join("open").join(&i.id).join(name);
    if !dest.exists() {
        restore_one(tg, &to_entry(i), &dest)?;
    }
    Ok(dest)
}

pub fn run_restore(cfg: Config, p: Shared, cancel: Arc<AtomicBool>) {
    begin(&p, "Restoring");
    let tg = logged(&cfg, &p);
    let entries = match fetch_index(&tg) {
        Ok((mut i, f)) => {
            i.extend(f);
            i
        }
        Err(e) => {
            say(&p, format!("Could not read index: {e}"));
            return end(&p);
        }
    };
    p.lock().unwrap().total = entries.len();
    say(&p, format!("Index has {} item(s)", entries.len()));
    let (mut got, mut skipped, mut failed) = (0, 0, 0);
    for (n, e) in entries.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        p.lock().unwrap().current = e.file_name.clone();
        // file names come from a remote index: never let them escape the restore dir
        let name = Path::new(&e.file_name).file_name().map(|s| s.to_owned());
        let bucket = Path::new(&e.bucket_name).file_name().map(|s| s.to_owned());
        let (Some(name), Some(bucket)) = (name, bucket) else {
            say(&p, format!("Skipped unsafe name {:?}", e.file_name));
            continue;
        };
        let dest = Path::new(&cfg.restore_dir).join(bucket).join(name);
        if dest.exists() {
            skipped += 1;
        } else {
            match restore_one(&tg, e, &dest) {
                Ok(()) => got += 1,
                Err(err) => {
                    say(&p, format!("FAILED {}: {err}", e.file_name));
                    failed += 1;
                }
            }
        }
        p.lock().unwrap().done = n + 1;
    }
    say(&p, format!("Restore done: {got} downloaded, {skipped} already present, {failed} failed"));
    end(&p);
}

fn restore_one(tg: &Tg, e: &IndexEntry, dest: &Path) -> Result<(), String> {
    if e.chunk_file_ids.is_none() && e.size as u64 > MAX_SINGLE {
        return Err("over 20 MB and stored unsplit: a bot cannot download it. Open it in the Telegram app".into());
    }
    let bot = e.upload_bot_index.unwrap_or(0).max(0) as usize % tg.bots();
    let mut out = Vec::new();
    match &e.chunk_file_ids {
        Some(ids) if e.telegram_file_id.starts_with(CHUNKED_PREFIX) => {
            for id in ids {
                out.extend(tg.download(bot, id)?);
            }
        }
        _ => out = tg.download(bot, &e.telegram_file_id)?,
    }
    if out.len() as i64 != e.size {
        return Err(format!("size mismatch: got {} expected {}", out.len(), e.size));
    }
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(|x| x.to_string())?;
    std::fs::write(dest, out).map_err(|x| x.to_string())
}

fn begin(p: &Shared, task: &str) {
    let mut g = p.lock().unwrap();
    g.running = true;
    g.task = task.into();
    g.done = 0;
    g.total = 0;
    g.current.clear();
}

fn end(p: &Shared) {
    let mut g = p.lock().unwrap();
    g.running = false;
    g.current.clear();
    g.version += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_keeps_other_files_out_of_items() {
        // index written by the Android app: no `files` key
        let old = r#"{"schemaVersion":2,"exportedAt":1,"items":[{"id":"a","telegramFileId":"f","telegramMessageId":1,"fileName":"a.jpg","mimeType":"image/jpeg","size":1,"dateTaken":1,"bucketName":"b","mediaType":"IMAGE"}]}"#;
        let m: Manifest = serde_json::from_str(old).unwrap();
        assert_eq!((m.items.len(), m.files.len()), (1, 0));

        let pdf = Item {
            id: "p".into(), path: "/x/a.pdf".into(), name: "a.pdf".into(), mime: "application/pdf".into(), size: 5, mtime: 1,
            bucket: "x".into(), video: false, other: true, status: "BACKED_UP".into(), file_id: Some("pf".into()),
            message_id: Some(2), bot: 0, chunk_ids: vec![], chunk_lens: vec![], chunk_msgs: vec![], thumb: None, error: None, thumb_file_id: None,
        };
        assert_eq!(to_entry(&pdf).media_type, "FILE");
        let out = serde_json::to_value(Manifest { schema_version: 2, exported_at: 1, items: m.items, files: vec![to_entry(&pdf)] }).unwrap();
        // the Android app only reads `items`, and every media type there is IMAGE or VIDEO
        assert!(out["items"].as_array().unwrap().iter().all(|e| matches!(e["mediaType"].as_str(), Some("IMAGE" | "VIDEO"))));
        assert_eq!(out["files"][0]["fileName"], "a.pdf");
    }
}
