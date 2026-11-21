use rusqlite::{params, Connection};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub path: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub mtime: i64,
    pub bucket: String,
    pub video: bool,
    pub status: String, // PENDING | BACKED_UP | FAILED
    pub file_id: Option<String>,
    pub message_id: Option<i64>,
    pub bot: i64,
    pub chunk_ids: Vec<String>,
    pub chunk_lens: Vec<i64>,
    pub chunk_msgs: Vec<i64>,
    pub thumb: Option<String>,
    pub error: Option<String>,
    /// Telegram file_id of a remote thumbnail (cloud items)
    pub thumb_file_id: Option<String>,
    /// not a photo or video (PDF, ebook, ...); listed under `files` in the index
    pub other: bool,
}

#[derive(Clone)]
pub struct Store(Arc<Mutex<Connection>>, Arc<Mutex<String>>);

const COLS: &str = "id,path,name,mime,size,mtime,bucket,video,status,file_id,message_id,bot,chunk_ids,chunk_lens,chunk_msgs,thumb,error,thumb_file_id,other";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Item> {
    let j = |i| -> String { r.get::<_, Option<String>>(i).ok().flatten().unwrap_or_else(|| "[]".into()) };
    Ok(Item {
        id: r.get(0)?, path: r.get(1)?, name: r.get(2)?, mime: r.get(3)?, size: r.get(4)?,
        mtime: r.get(5)?, bucket: r.get(6)?, video: r.get::<_, i64>(7)? != 0, status: r.get(8)?,
        file_id: r.get(9)?, message_id: r.get(10)?, bot: r.get(11)?,
        chunk_ids: serde_json::from_str(&j(12)).unwrap_or_default(),
        chunk_lens: serde_json::from_str(&j(13)).unwrap_or_default(),
        chunk_msgs: serde_json::from_str(&j(14)).unwrap_or_default(),
        thumb: r.get(15)?, error: r.get(16)?, thumb_file_id: r.get(17)?, other: r.get::<_, i64>(18)? != 0,
    })
}

impl Store {
    /// Items shown, scanned and uploaded belong to this chat.
    pub fn set_chat(&self, chat: &str) {
        *self.1.lock().unwrap() = chat.to_string();
    }

    fn chat(&self) -> String {
        self.1.lock().unwrap().clone()
    }

    /// Rows written before multi-chat belong to the chat that was configured then.
    pub fn adopt_legacy(&self, chat: &str) {
        self.0.lock().unwrap().execute("UPDATE media_items SET chat=? WHERE chat=''", params![chat]).ok();
    }

    /// Forget a chat's rows (Telegram data is untouched).
    pub fn delete_chat(&self, chat: &str) {
        self.0.lock().unwrap().execute("DELETE FROM media_items WHERE chat=?", params![chat]).ok();
    }

    pub fn open() -> Store {
        let c = Connection::open(crate::config::data_dir().join("ulap.db")).expect("open db");
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS media_items(
              id TEXT PRIMARY KEY, path TEXT NOT NULL, name TEXT NOT NULL, mime TEXT NOT NULL,
              size INTEGER NOT NULL, mtime INTEGER NOT NULL, bucket TEXT NOT NULL, video INTEGER NOT NULL,
              status TEXT NOT NULL DEFAULT 'PENDING', file_id TEXT, message_id INTEGER,
              bot INTEGER NOT NULL DEFAULT 0, chunk_ids TEXT, chunk_lens TEXT, chunk_msgs TEXT,
              thumb TEXT, error TEXT);
             CREATE INDEX IF NOT EXISTS idx_status ON media_items(status);
             CREATE INDEX IF NOT EXISTS idx_mtime ON media_items(mtime);",
        )
        .expect("schema");
        // older databases: add the column (error if it already exists is fine)
        c.execute("ALTER TABLE media_items ADD COLUMN thumb_file_id TEXT", []).ok();
        c.execute("ALTER TABLE media_items ADD COLUMN other INTEGER NOT NULL DEFAULT 0", []).ok();
        c.execute("ALTER TABLE media_items ADD COLUMN chat TEXT NOT NULL DEFAULT ''", []).ok();
        c.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_chat ON media_items(chat);
             CREATE INDEX IF NOT EXISTS idx_fileid ON media_items(file_id);
             CREATE INDEX IF NOT EXISTS idx_name_size ON media_items(name,size);",
        )
        .expect("indexes");
        Store(Arc::new(Mutex::new(c)), Arc::new(Mutex::new(String::new())))
    }

    /// Insert a scanned file; keeps backup state when the id (path+size+mtime) is unchanged.
    pub fn insert_if_new(&self, i: &Item) -> bool {
        let chat = self.chat();
        let c = self.0.lock().unwrap();
        c.execute(
            "INSERT OR IGNORE INTO media_items(id,path,name,mime,size,mtime,bucket,video,chat,other) VALUES(?,?,?,?,?,?,?,?,?,?)",
            params![i.id, i.path, i.name, i.mime, i.size, i.mtime, i.bucket, i.video as i64, chat, i.other as i64],
        )
        .unwrap_or(0)
            > 0
    }

    /// Insert items known only from the Telegram index (no local file), in one transaction.
    /// Skips ones this PC uploaded itself.
    pub fn upsert_remote_batch(&self, items: &[Item]) {
        let chat = self.chat();
        let mut c = self.0.lock().unwrap();
        let Ok(tx) = c.transaction() else { return };
        for i in items {
            let known = tx.query_row("SELECT 1 FROM media_items WHERE file_id=? AND chat=?", params![i.file_id, chat], |_| Ok(())).is_ok();
            if known {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO media_items(id,path,name,mime,size,mtime,bucket,video,status,file_id,message_id,bot,chunk_ids,chunk_lens,chunk_msgs,thumb_file_id,chat,other)
                 VALUES(?,?,?,?,?,?,?,?,'CLOUD_ONLY',?,?,?,?,?,?,?,?,?)",
                params![
                    i.id, "", i.name, i.mime, i.size, i.mtime, i.bucket, i.video as i64, i.file_id, i.message_id, i.bot,
                    serde_json::to_string(&i.chunk_ids).unwrap(),
                    serde_json::to_string(&i.chunk_lens).unwrap(),
                    serde_json::to_string(&i.chunk_msgs).unwrap(),
                    i.thumb_file_id, chat, i.other as i64
                ],
            )
            .ok();
        }
        tx.commit().ok();
    }

    /// True when the Telegram index already holds a file with this name and size (avoids re-uploading restored files).
    pub fn has_remote(&self, name: &str, size: i64) -> bool {
        let chat = self.chat();
        self.0
            .lock()
            .unwrap()
            .query_row("SELECT 1 FROM media_items WHERE status='CLOUD_ONLY' AND name=? AND size=? AND chat=?", params![name, size, chat], |_| Ok(()))
            .is_ok()
    }

    pub fn all(&self) -> Vec<Item> {
        let chat = self.chat();
        let c = self.0.lock().unwrap();
        let mut s = c.prepare(&format!("SELECT {COLS} FROM media_items WHERE chat=? ORDER BY mtime DESC")).unwrap();
        s.query_map(params![chat], row).unwrap().flatten().collect()
    }

    pub fn pending(&self) -> Vec<Item> {
        let chat = self.chat();
        let c = self.0.lock().unwrap();
        let mut s = c
            .prepare(&format!("SELECT {COLS} FROM media_items WHERE chat=? AND status IN ('PENDING','FAILED') ORDER BY mtime DESC"))
            .unwrap();
        s.query_map(params![chat], row).unwrap().flatten().collect()
    }

    /// Drop rows (cloud items deleted from Telegram).
    pub fn delete_ids(&self, ids: &[String]) {
        let mut c = self.0.lock().unwrap();
        if let Ok(tx) = c.transaction() {
            for id in ids {
                tx.execute("DELETE FROM media_items WHERE id=?", params![id]).ok();
            }
            tx.commit().ok();
        }
    }

    /// A file the user explicitly dropped: upload again if it had been excluded.
    pub fn revive(&self, id: &str) {
        self.0.lock().unwrap().execute("UPDATE media_items SET status='PENDING' WHERE id=? AND status='EXCLUDED'", params![id]).ok();
    }

    pub fn set_status(&self, id: &str, status: &str) {
        self.0.lock().unwrap().execute("UPDATE media_items SET status=?,error=NULL WHERE id=?", params![status, id]).ok();
    }

    /// Local file stays, but the Telegram copy is gone: never upload it again unless asked.
    pub fn exclude(&self, id: &str) {
        self.0
            .lock()
            .unwrap()
            .execute(
                "UPDATE media_items SET status='EXCLUDED',file_id=NULL,message_id=NULL,chunk_ids='[]',chunk_lens='[]',chunk_msgs='[]' WHERE id=?",
                params![id],
            )
            .ok();
    }

    /// Local file was removed to free space; the Telegram copy remains.
    pub fn drop_local(&self, id: &str) {
        self.0.lock().unwrap().execute("UPDATE media_items SET path='',status='CLOUD_ONLY' WHERE id=?", params![id]).ok();
    }

    pub fn set_thumb(&self, id: &str, thumb: &str) {
        self.0.lock().unwrap().execute("UPDATE media_items SET thumb=? WHERE id=?", params![thumb, id]).ok();
    }

    pub fn set_failed(&self, id: &str, err: &str) {
        self.0.lock().unwrap().execute("UPDATE media_items SET status='FAILED',error=? WHERE id=?", params![err, id]).ok();
    }

    pub fn set_done(&self, i: &Item) {
        self.0
            .lock()
            .unwrap()
            .execute(
                "UPDATE media_items SET status='BACKED_UP',error=NULL,size=?,file_id=?,message_id=?,bot=?,chunk_ids=?,chunk_lens=?,chunk_msgs=? WHERE id=?",
                params![
                    i.size, i.file_id, i.message_id, i.bot,
                    serde_json::to_string(&i.chunk_ids).unwrap(),
                    serde_json::to_string(&i.chunk_lens).unwrap(),
                    serde_json::to_string(&i.chunk_msgs).unwrap(),
                    i.id
                ],
            )
            .ok();
    }
}
