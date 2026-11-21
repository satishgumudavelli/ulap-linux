//! `ulap-account`: helper process for the optional Telegram *user account* login (MTProto via
//! grammers). It is a separate program because grammers bundles its own SQLite, which cannot be
//! linked into the main app next to rusqlite. Protocol: JSON lines. stdin: commands; stdout: state.
//! usage: ulap-account <api_id> <session_path>   (api hash is read from the first stdin line)
use grammers_client::{client::SignInError, Client};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use grammers_tl_types as tl;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::Arc;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

#[derive(Default)]
struct State {
    stage: &'static str, // connecting | need_phone | need_code | need_password | ready
    hint: String,
    name: String,
    msg: String,
    created: Option<Result<(String, String), String>>,
    logged_out: bool,
}

fn emit(s: &mut State) {
    let created = match s.created.take() {
        Some(Ok((id, title))) => json!({"ok": true, "id": id, "title": title}),
        Some(Err(e)) => json!({"ok": false, "err": e}),
        None => Value::Null,
    };
    let v = json!({"stage": s.stage, "hint": s.hint, "name": s.name, "msg": s.msg, "created": created, "logged_out": s.logged_out});
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").ok();
    out.flush().ok();
}

fn set(s: &mut State, f: impl FnOnce(&mut State)) {
    f(s);
    emit(s);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(api_id), Some(path)) = (args.get(1).and_then(|a| a.parse::<i32>().ok()), args.get(2).cloned()) else {
        eprintln!("usage: ulap-account <api_id> <session_path>");
        std::process::exit(2);
    };
    let stdin = std::io::stdin();
    let mut first = String::new();
    stdin.lock().read_line(&mut first).ok();
    let api_hash = first.trim().to_string();

    let (tx, rx) = unbounded_channel::<Value>();
    std::thread::spawn(move || {
        for line in stdin.lock().lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                tx.send(v).ok();
            }
        }
        tx.send(json!({"cmd": "quit"})).ok(); // parent closed the pipe
    });
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(run(api_id, api_hash, path, rx));
}

async fn run(api_id: i32, api_hash: String, path: String, mut rx: UnboundedReceiver<Value>) {
    let mut st = State { stage: "connecting", ..Default::default() };
    emit(&mut st);
    let session = match SqliteSession::open(&path).await {
        Ok(s) => Arc::new(s),
        Err(e) => return set(&mut st, |s| s.msg = format!("Cannot open session: {e}")),
    };
    let SenderPool { runner, updates: _updates, handle } = SenderPool::new(session, api_id);
    let client = Client::new(handle.clone());
    let pool = tokio::spawn(runner.run());

    match client.is_authorized().await {
        Ok(true) => {
            let name = client.get_me().await.map(|u| u.full_name()).unwrap_or_else(|_| "your account".into());
            set(&mut st, |s| {
                s.stage = "ready";
                s.name = name
            });
        }
        Ok(false) => set(&mut st, |s| s.stage = "need_phone"),
        Err(e) => return set(&mut st, |s| s.msg = format!("Cannot reach Telegram: {e}")),
    }

    let mut token = None;
    let mut pw_token = None;
    while let Some(cmd) = rx.recv().await {
        let v = cmd["v"].as_str().unwrap_or("").to_string();
        match cmd["cmd"].as_str().unwrap_or("") {
            "phone" => match client.request_login_code(v.trim(), &api_hash).await {
                Ok(t) => {
                    token = Some(t);
                    set(&mut st, |s| {
                        s.stage = "need_code";
                        s.msg = "Code sent to your Telegram app (or SMS).".into()
                    })
                }
                Err(e) => set(&mut st, |s| s.msg = format!("Could not send code: {e}")),
            },
            "code" => {
                let Some(t) = &token else { continue };
                match client.sign_in(t, v.trim()).await {
                    Ok(u) => set(&mut st, |s| {
                        s.stage = "ready";
                        s.name = u.full_name();
                        s.msg.clear()
                    }),
                    Err(SignInError::PasswordRequired(pt)) => {
                        let hint = pt.hint().unwrap_or("").to_string();
                        pw_token = Some(pt);
                        set(&mut st, |s| {
                            s.stage = "need_password";
                            s.hint = hint;
                            s.msg = "Two-step password required.".into()
                        })
                    }
                    Err(e) => set(&mut st, |s| s.msg = e.to_string()),
                }
            }
            "password" => {
                let Some(pt) = pw_token.take() else { continue };
                match client.check_password(pt, v.as_bytes()).await {
                    Ok(u) => set(&mut st, |s| {
                        s.stage = "ready";
                        s.name = u.full_name();
                        s.msg.clear()
                    }),
                    Err(SignInError::InvalidPassword(pt)) => {
                        let hint = pt.hint().unwrap_or("").to_string();
                        pw_token = Some(pt);
                        set(&mut st, |s| {
                            s.stage = "need_password";
                            s.hint = hint;
                            s.msg = "Wrong password, try again.".into()
                        })
                    }
                    Err(e) => set(&mut st, |s| s.msg = e.to_string()),
                }
            }
            "create" => {
                let title = cmd["title"].as_str().unwrap_or("Ulap Photos").to_string();
                let bot = cmd["bot"].as_str().unwrap_or("").to_string();
                let r = create_channel(&client, &title, &bot).await;
                set(&mut st, |s| s.created = Some(r));
            }
            "logout" => {
                client.sign_out().await.ok();
                pool.abort();
                for ext in ["", "-wal", "-shm"] {
                    std::fs::remove_file(format!("{path}{ext}")).ok();
                }
                return set(&mut st, |s| {
                    s.logged_out = true;
                    s.stage = "need_phone";
                    s.msg = "Logged out; session deleted.".into()
                });
            }
            "quit" => return,
            _ => {}
        }
    }
}

/// Create a private broadcast channel and make the bot an admin. Returns (bot-api chat id, title).
async fn create_channel(client: &Client, title: &str, bot_username: &str) -> Result<(String, String), String> {
    // 1. find the bot first, so we fail before creating anything
    let resolved = client
        .invoke(&tl::functions::contacts::ResolveUsername { username: bot_username.trim_start_matches('@').to_string(), referer: None })
        .await
        .map_err(|e| format!("Cannot find the bot @{bot_username}: {e}"))?;
    let tl::enums::contacts::ResolvedPeer::Peer(rp) = resolved;
    let bot = rp
        .users
        .iter()
        .find_map(|u| match u {
            tl::enums::User::User(u) if u.bot => Some((u.id, u.access_hash)),
            _ => None,
        })
        .ok_or("That username is not a bot")?;
    let bot_hash = bot.1.ok_or("Telegram gave no access hash for the bot")?;

    // 2. create the channel (private: no username is set)
    let updates = client
        .invoke(&tl::functions::channels::CreateChannel {
            broadcast: true,
            megagroup: false,
            for_import: false,
            forum: false,
            title: title.to_string(),
            about: "Ulap photo backup".to_string(),
            geo_point: None,
            address: None,
            ttl_period: None,
        })
        .await
        .map_err(|e| format!("Could not create the channel: {e}"))?;
    let chats = match updates {
        tl::enums::Updates::Updates(u) => u.chats,
        tl::enums::Updates::Combined(u) => u.chats,
        _ => vec![],
    };
    let (cid, chash) = chats
        .iter()
        .find_map(|c| match c {
            tl::enums::Chat::Channel(c) => Some((c.id, c.access_hash)),
            _ => None,
        })
        .ok_or("Channel created but Telegram did not return it")?;
    let chash = chash.ok_or("Telegram gave no access hash for the channel")?;

    // 3. add the bot as admin with the rights Ulap needs
    let rights = tl::types::ChatAdminRights {
        change_info: false,
        post_messages: true,
        edit_messages: true,
        delete_messages: true,
        ban_users: false,
        invite_users: false,
        pin_messages: true,
        add_admins: false,
        anonymous: false,
        manage_call: false,
        other: false,
        manage_topics: false,
        post_stories: false,
        edit_stories: false,
        delete_stories: false,
        manage_direct_messages: false,
        manage_ranks: false,
        manage_linked_peers: false,
        manage_welcome_messages: false,
    };
    client
        .invoke(&tl::functions::channels::EditAdmin {
            channel: tl::enums::InputChannel::Channel(tl::types::InputChannel { channel_id: cid, access_hash: chash }),
            user_id: tl::enums::InputUser::User(tl::types::InputUser { user_id: bot.0, access_hash: bot_hash }),
            admin_rights: rights.into(),
            rank: Some(String::new()),
        })
        .await
        .map_err(|e| format!("Channel \"{title}\" was created but adding the bot failed: {e}. Add the bot as admin manually."))?;

    Ok((format!("-100{cid}"), title.to_string()))
}
