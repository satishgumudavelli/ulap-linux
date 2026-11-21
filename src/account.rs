//! Client side of the optional Telegram *user account* login. The MTProto work happens in the
//! `ulap-account` helper process (see src/bin/ulap-account.rs); this talks to it over JSON lines.
//! The session file gives full access to the account: it lives in an owner-only directory and
//! "Log out" deletes it.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default, PartialEq, Debug)]
pub enum Stage {
    #[default]
    Connecting,
    NeedPhone,
    NeedCode,
    NeedPassword(String),
    Ready(String),
}

#[derive(Default)]
pub struct State {
    pub stage: Stage,
    pub busy: bool,
    pub msg: String,
    /// (bot-api chat id, title) of a channel just created, or an error
    pub created: Option<Result<(String, String), String>>,
    /// helper finished (logged out) or died: drop the Account
    pub logged_out: bool,
}

pub enum Cmd {
    Phone(String),
    Code(String),
    Password(String),
    CreateChannel { title: String, bot_username: String },
    Logout,
}

#[derive(Clone)]
pub struct Account {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    pub st: Arc<Mutex<State>>,
}

pub fn session_path() -> PathBuf {
    crate::config::data_dir().join("account.session")
}

fn lock_down_dir() {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(crate::config::data_dir(), std::fs::Permissions::from_mode(0o700)).ok();
}

fn helper_path() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ulap-account"))).unwrap_or_else(|| "ulap-account".into())
}

impl Account {
    pub fn send(&self, c: Cmd) {
        let line = match c {
            Cmd::Phone(v) => json!({"cmd": "phone", "v": v}),
            Cmd::Code(v) => json!({"cmd": "code", "v": v}),
            Cmd::Password(v) => json!({"cmd": "password", "v": v}),
            Cmd::CreateChannel { title, bot_username } => json!({"cmd": "create", "title": title, "bot": bot_username}),
            Cmd::Logout => json!({"cmd": "logout"}),
        };
        self.st.lock().unwrap().busy = true;
        if let Some(w) = self.stdin.lock().unwrap().as_mut() {
            if writeln!(w, "{line}").and_then(|_| w.flush()).is_err() {
                let mut g = self.st.lock().unwrap();
                g.busy = false;
                g.msg = "Account helper is not running.".into();
            }
        }
    }

    pub fn start(api_id: i32, api_hash: String) -> Account {
        lock_down_dir();
        let st = Arc::new(Mutex::new(State::default()));
        let stdin = Arc::new(Mutex::new(None));
        let mut child = match Command::new(helper_path())
            .arg(api_id.to_string())
            .arg(session_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                {
                    let mut g = st.lock().unwrap();
                    g.msg = format!("Cannot start the account helper (ulap-account): {e}");
                    g.logged_out = true;
                }
                return Account { stdin, st };
            }
        };
        let mut w = child.stdin.take().expect("piped stdin");
        writeln!(w, "{api_hash}").and_then(|_| w.flush()).ok(); // hash goes over the pipe, never argv
        *stdin.lock().unwrap() = Some(w);
        let out = child.stdout.take().expect("piped stdout");
        let st2 = st.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let mut g = st2.lock().unwrap();
                g.busy = false;
                g.stage = match v["stage"].as_str().unwrap_or("") {
                    "need_phone" => Stage::NeedPhone,
                    "need_code" => Stage::NeedCode,
                    "need_password" => Stage::NeedPassword(v["hint"].as_str().unwrap_or("").into()),
                    "ready" => Stage::Ready(v["name"].as_str().unwrap_or("").into()),
                    _ => Stage::Connecting,
                };
                g.msg = v["msg"].as_str().unwrap_or("").into();
                if v["logged_out"].as_bool() == Some(true) {
                    g.logged_out = true;
                }
                if !v["created"].is_null() {
                    g.created = Some(if v["created"]["ok"].as_bool() == Some(true) {
                        Ok((v["created"]["id"].as_str().unwrap_or("").into(), v["created"]["title"].as_str().unwrap_or("").into()))
                    } else {
                        Err(v["created"]["err"].as_str().unwrap_or("failed").into())
                    });
                }
            }
            // helper exited
            let mut g = st2.lock().unwrap();
            if !g.logged_out && g.msg.is_empty() {
                g.msg = "Account helper stopped.".into();
            }
            g.busy = false;
            g.logged_out = true;
            child.wait().ok();
        });
        Account { stdin, st }
    }
}
