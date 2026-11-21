//! Telegram Bot API client. Layout constants match the Android app (ADR-004).
use reqwest::blocking::{multipart, Client};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CHUNK_SIZE: u64 = 19 * 1024 * 1024;
pub const FAST_START_CHUNK: u64 = 512 * 1024;
pub const MAX_SINGLE: u64 = 20 * 1024 * 1024;
/// Bot API upload limit for one document. (Downloading is limited to MAX_SINGLE, uploading is not.)
pub const BOT_UPLOAD_MAX: u64 = 50_000_000;
pub const CHUNKED_PREFIX: &str = "chunked:";
const MAX_RETRIES: u32 = 5;

pub struct Doc {
    pub file_id: String,
    pub message_id: i64,
}

pub struct Tg {
    tokens: Vec<String>,
    chat: String,
    http: Client,
    /// per-bot rate-limit cooldown end
    cool: Mutex<Vec<Option<Instant>>>,
    logger: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

pub fn sanitize_token(t: &str) -> String {
    t.trim().replace(':', "%3A")
}

/// Byte length of each chunk of a chunked upload (port of `ChunkMetadataLayout`).
pub fn chunk_lengths(total: u64) -> Vec<u64> {
    if total <= FAST_START_CHUNK {
        return vec![total];
    }
    let mut out = vec![FAST_START_CHUNK];
    let mut left = total - FAST_START_CHUNK;
    while left > 0 {
        let n = left.min(CHUNK_SIZE);
        out.push(n);
        left -= n;
    }
    out
}

impl Tg {
    pub fn new(tokens: &[String], chat: &str) -> Tg {
        let http = Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("http client");
        Tg {
            tokens: tokens.iter().map(|t| sanitize_token(t)).collect(),
            chat: chat.trim().into(),
            http,
            cool: Mutex::new(vec![None; tokens.len()]),
            logger: None,
        }
    }

    pub fn with_logger(mut self, f: Arc<dyn Fn(String) + Send + Sync>) -> Tg {
        self.logger = Some(f);
        self
    }

    fn log(&self, s: String) {
        if let Some(l) = &self.logger {
            l(s)
        }
    }

    /// Next bot for upload number `n`: round-robin, skipping bots that are rate-limited right now.
    pub fn pick_bot(&self, n: usize) -> usize {
        let cool = self.cool.lock().unwrap();
        let now = Instant::now();
        let free = |b: usize| cool[b].map_or(true, |t| t <= now);
        let k = cool.len();
        if let Some(b) = (0..k).map(|i| (n + i) % k).find(|b| free(*b)) {
            return b;
        }
        // all cooling: the one that frees up first (the call will wait out the rest)
        (0..k).min_by_key(|b| cool[*b]).unwrap_or(0)
    }

    pub fn bots(&self) -> usize {
        self.tokens.len()
    }

    fn url(&self, bot: usize, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.tokens[bot % self.tokens.len()], method)
    }

    /// Runs `build` until ok. Handles 429 (retry_after) and transient failures with backoff.
    fn call(&self, bot: usize, method: &str, build: impl Fn() -> reqwest::blocking::RequestBuilder) -> Result<Value, String> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = build().send().and_then(|r| r.json::<Value>());
            match resp {
                Ok(v) if v["ok"].as_bool() == Some(true) => return Ok(v["result"].clone()),
                Ok(v) => {
                    let code = v["error_code"].as_i64().unwrap_or(0);
                    let desc = v["description"].as_str().unwrap_or("unknown error").to_string();
                    if code == 429 {
                        let wait = v["parameters"]["retry_after"].as_u64().unwrap_or(30) + 1;
                        if let Some(c) = self.cool.lock().unwrap().get_mut(bot % self.tokens.len()) {
                            *c = Some(Instant::now() + Duration::from_secs(wait));
                        }
                        self.log(format!("Rate limited (429) on {method}, bot {}: waiting {wait}s", bot % self.tokens.len() + 1));
                        std::thread::sleep(Duration::from_secs(wait));
                        continue;
                    }
                    if code == 401 || code == 403 || attempt >= MAX_RETRIES {
                        return Err(format!("Telegram {code}: {desc}"));
                    }
                }
                Err(e) if attempt >= MAX_RETRIES => return Err(e.to_string().replace(&self.tokens[0], "<token>")),
                Err(_) => {}
            }
            let base = (3_000u64 << (attempt - 1)).min(60_000);
            std::thread::sleep(Duration::from_millis(base));
        }
    }

    pub fn get_me(&self, bot: usize) -> Result<String, String> {
        let r = self.call(bot, "getMe", || self.http.get(self.url(bot, "getMe")))?;
        Ok(r["username"].as_str().unwrap_or("bot").to_string())
    }

    pub fn send_document(&self, bot: usize, name: &str, data: Vec<u8>, caption: &str) -> Result<Doc, String> {
        let r = self.call(bot, "sendDocument", || {
            let part = multipart::Part::bytes(data.clone()).file_name(name.to_string());
            let form = multipart::Form::new()
                .text("chat_id", self.chat.clone())
                .text("caption", caption.to_string())
                .part("document", part);
            self.http.post(self.url(bot, "sendDocument")).multipart(form)
        })?;
        let file_id = r["document"]["file_id"].as_str().ok_or("No file_id in response")?.to_string();
        Ok(Doc { file_id, message_id: r["message_id"].as_i64().unwrap_or(0) })
    }

    pub fn download(&self, bot: usize, file_id: &str) -> Result<Vec<u8>, String> {
        let r = self.call(bot, "getFile", || {
            self.http.get(self.url(bot, "getFile")).query(&[("file_id", file_id)])
        })?;
        let path = r["file_path"].as_str().ok_or("No file_path")?;
        let url = format!("https://api.telegram.org/file/bot{}/{}", self.tokens[bot % self.tokens.len()], path);
        let resp = self.http.get(url).send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("download failed: HTTP {}", resp.status()));
        }
        resp.bytes().map(|b| b.to_vec()).map_err(|e| e.to_string())
    }

    /// Chats this bot has recently seen (from pending updates): (chat_id, title, type).
    pub fn recent_chats(&self, bot: usize) -> Result<Vec<(i64, String, String)>, String> {
        let r = self.call(bot, "getUpdates", || self.http.get(self.url(bot, "getUpdates")).query(&[("limit", "100")]))?;
        let mut out: Vec<(i64, String, String)> = vec![];
        for u in r.as_array().cloned().unwrap_or_default() {
            for key in ["message", "channel_post", "edited_message", "edited_channel_post", "my_chat_member"] {
                let c = &u[key]["chat"];
                let Some(id) = c["id"].as_i64() else { continue };
                if out.iter().any(|(i, _, _)| *i == id) {
                    continue;
                }
                let title = c["title"].as_str().or(c["username"].as_str()).or(c["first_name"].as_str()).unwrap_or("(no name)");
                out.push((id, title.to_string(), c["type"].as_str().unwrap_or("").to_string()));
            }
        }
        Ok(out)
    }

    /// Delete messages (up to 100 per API call). Missing messages are ignored by Telegram.
    pub fn delete_messages(&self, bot: usize, ids: &[i64]) -> Result<(), String> {
        for batch in ids.chunks(100) {
            let json = serde_json::to_string(batch).unwrap();
            self.call(bot, "deleteMessages", || {
                self.http.post(self.url(bot, "deleteMessages")).form(&[
                    ("chat_id", self.chat.clone()),
                    ("message_ids", json.clone()),
                ])
            })?;
        }
        Ok(())
    }

    /// Name of the configured chat; also proves the bot can reach it.
    pub fn chat_title(&self, bot: usize) -> Result<String, String> {
        let r = self.call(bot, "getChat", || {
            self.http.get(self.url(bot, "getChat")).query(&[("chat_id", self.chat.as_str())])
        })?;
        Ok(r["title"].as_str().or(r["username"].as_str()).or(r["first_name"].as_str()).unwrap_or("Chat").to_string())
    }

    pub fn pin(&self, bot: usize, message_id: i64) -> Result<(), String> {
        self.call(bot, "pinChatMessage", || {
            self.http.post(self.url(bot, "pinChatMessage")).form(&[
                ("chat_id", self.chat.clone()),
                ("message_id", message_id.to_string()),
                ("disable_notification", "true".into()),
            ])
        })
        .map(|_| ())
    }

    /// (file_id, caption) of the chat's pinned document, if any.
    pub fn pinned_document(&self, bot: usize) -> Result<Option<(String, String)>, String> {
        let r = self.call(bot, "getChat", || {
            self.http.get(self.url(bot, "getChat")).query(&[("chat_id", self.chat.as_str())])
        })?;
        let m = &r["pinned_message"];
        Ok(m["document"]["file_id"]
            .as_str()
            .map(|f| (f.to_string(), m["caption"].as_str().unwrap_or("").to_string())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_android() {
        assert_eq!(chunk_lengths(100), vec![100]);
        assert_eq!(chunk_lengths(FAST_START_CHUNK), vec![FAST_START_CHUNK]);
        let t = FAST_START_CHUNK + CHUNK_SIZE + 5;
        assert_eq!(chunk_lengths(t), vec![FAST_START_CHUNK, CHUNK_SIZE, 5]);
        assert_eq!(chunk_lengths(t).iter().sum::<u64>(), t);
    }

    #[test]
    fn rate_limited_bot_is_skipped() {
        let tg = Tg::new(&["a".into(), "b".into()], "1");
        assert_eq!(tg.pick_bot(0), 0);
        tg.cool.lock().unwrap()[0] = Some(Instant::now() + Duration::from_secs(60));
        assert_eq!(tg.pick_bot(0), 1);
        tg.cool.lock().unwrap()[1] = Some(Instant::now() + Duration::from_secs(5));
        assert_eq!(tg.pick_bot(0), 1); // both cooling: earliest to free up
    }

    #[test]
    fn token_path() {
        assert_eq!(sanitize_token(" 12:AB\n"), "12%3AAB");
    }
}
