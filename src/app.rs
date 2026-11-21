use crate::account::{self, Account, Stage};
use crate::config::{Config, SavedChat};
use crate::store::{Item, Store};
use crate::sync::{self, Progress, Shared};
use eframe::egui;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Gallery,
    Settings,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Sync,
    Restore,
    Refresh,
}

#[derive(PartialEq, Clone, Copy)]
enum Filter {
    All,
    Photos,
    Videos,
    Files,
}

struct Wizard {
    username: Arc<Mutex<String>>,
    status: Arc<Mutex<String>>,
    cancel: Arc<AtomicBool>,
}

pub struct App {
    cfg: Config,
    store: Store,
    prog: Shared,
    cancel: Arc<AtomicBool>,
    tab: Tab,
    filter: Filter,
    items: Vec<Item>,
    seen_version: u64,
    primary: String,
    new_bot: String,
    confirm_free: bool,
    folder_input: String,
    notice: Arc<Mutex<String>>,
    thumb_tx: Option<mpsc::Sender<Item>>,
    requested: HashSet<String>,
    chats: Arc<Mutex<Vec<(i64, String, String)>>>,
    started: bool,
    select_mode: bool,
    selected: HashSet<String>,
    confirm_delete: Option<Vec<Item>>,
    confirm_restore: bool,
    add_id: String,
    add_name: String,
    pending_chat: Arc<Mutex<Option<Result<SavedChat, String>>>>,
    wizard: Option<Wizard>,
    account: Option<Account>,
    phone: String,
    code: String,
    password: String,
    new_chat_name: String,
    auto_activate: bool,
    started_account: bool,
}

impl App {
    pub fn new() -> App {
        let cfg = Config::load();
        let store = Store::open();
        store.set_chat(&cfg.chat_id);
        if !cfg.chat_id.is_empty() {
            store.adopt_legacy(&cfg.chat_id);
        }
        let tab = if cfg.ready() { Tab::Gallery } else { Tab::Settings };
        App {
            primary: cfg.tokens.first().cloned().unwrap_or_default(),
            new_bot: String::new(),
            confirm_free: false,
            items: store.all(),
            cfg,
            store,
            prog: Arc::new(Mutex::new(Progress::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            tab,
            filter: Filter::All,
            seen_version: 0,
            folder_input: String::new(),
            notice: Arc::new(Mutex::new(String::new())),
            thumb_tx: None,
            requested: HashSet::new(),
            chats: Arc::new(Mutex::new(vec![])),
            started: false,
            select_mode: false,
            selected: HashSet::new(),
            confirm_delete: None,
            confirm_restore: false,
            add_id: String::new(),
            add_name: String::new(),
            pending_chat: Arc::new(Mutex::new(None)),
            wizard: None,
            account: None,
            phone: String::new(),
            code: String::new(),
            password: String::new(),
            new_chat_name: "Ulap Photos".into(),
            auto_activate: false,
            started_account: false,
        }
    }

    /// Make `id` the active chat: its own gallery, uploads and restore.
    fn activate(&mut self, id: &str) {
        if self.running() {
            return;
        }
        self.cfg.chat_id = id.to_string();
        self.cfg.save().ok();
        self.store.set_chat(id);
        self.thumb_tx = None;
        self.requested.clear();
        self.selected.clear();
        self.items = self.store.all();
        if self.cfg.ready() {
            self.start(Kind::Refresh);
        }
    }

    /// Guided chat creation: the user creates the chat in Telegram and adds the bot as admin via an
    /// official deep link; this thread watches for the bot's new chat and hands it back.
    fn start_wizard(&mut self) {
        let token = self.primary.trim().to_string();
        if token.is_empty() {
            *self.notice.lock().unwrap() = "Enter and save the bot token first.".into();
            return;
        }
        let w = Wizard { username: Arc::new(Mutex::new(String::new())), status: Arc::new(Mutex::new("Preparing…".into())), cancel: Arc::new(AtomicBool::new(false)) };
        let (user, status, cancel, out) = (w.username.clone(), w.status.clone(), w.cancel.clone(), self.pending_chat.clone());
        let known: HashSet<i64> = self.cfg.chats.iter().filter_map(|c| c.id.parse().ok()).collect();
        std::thread::spawn(move || {
            let tg = crate::telegram::Tg::new(&[token], "");
            match tg.get_me(0) {
                Ok(u) => *user.lock().unwrap() = u,
                Err(e) => {
                    *out.lock().unwrap() = Some(Err(format!("Bot check failed: {e}")));
                    return;
                }
            }
            // chats the bot already knows about are not the new one
            let mut baseline = known;
            match tg.recent_chats(0) {
                Ok(c) => baseline.extend(c.into_iter().map(|(id, _, _)| id)),
                Err(e) => {
                    *out.lock().unwrap() = Some(Err(format!("Cannot watch for new chats: {e}")));
                    return;
                }
            }
            *status.lock().unwrap() = "Waiting for you to add the bot in Telegram…".into();
            for _ in 0..200 {
                // up to ~10 minutes
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
                match tg.recent_chats(0) {
                    Ok(c) => {
                        if let Some((id, title, _)) = c.into_iter().find(|(id, _, t)| t != "private" && !baseline.contains(id)) {
                            *out.lock().unwrap() = Some(Ok(SavedChat { id: id.to_string(), name: title }));
                            return;
                        }
                    }
                    Err(e) => {
                        *out.lock().unwrap() = Some(Err(format!("Watching failed: {e}")));
                        return;
                    }
                }
            }
            *out.lock().unwrap() = Some(Err("Timed out waiting for the bot to be added.".into()));
        });
        self.wizard = Some(w);
    }

    fn wizard_dialog(&mut self, ctx: &egui::Context) {
        let Some(w) = &self.wizard else { return };
        let user = w.username.lock().unwrap().clone();
        let status = w.status.lock().unwrap().clone();
        let mut close = false;
        #[allow(deprecated)]
        let modal = egui::Modal::new(egui::Id::new("chat_wizard"));
        modal.show(ctx, |ui| {
            ui.set_max_width(520.0);
            ui.heading("Create a new chat");
            ui.label("1. In Telegram, create a new private channel (or group) with any name, for example \"Ulap Photos\".");
            if ui.button("Open Telegram").clicked() {
                std::process::Command::new("xdg-open").arg("https://web.telegram.org/").spawn().ok();
            }
            ui.label("2. Add your bot to it as admin. Telegram asks you to pick the chat and confirm:");
            ui.horizontal(|ui| {
                let ready = !user.is_empty();
                if ui.add_enabled(ready, egui::Button::new("Add bot to a channel")).clicked() {
                    let url = format!("https://t.me/{user}?startchannel&admin=post_messages+edit_messages+delete_messages+pin_messages");
                    std::process::Command::new("xdg-open").arg(url).spawn().ok();
                }
                if ui.add_enabled(ready, egui::Button::new("Add bot to a group")).clicked() {
                    let url = format!("https://t.me/{user}?startgroup&admin=delete_messages+pin_messages");
                    std::process::Command::new("xdg-open").arg(url).spawn().ok();
                }
            });
            ui.label("3. This window notices the new chat and adds it automatically.");
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(status);
            });
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
        if close {
            if let Some(w) = self.wizard.take() {
                w.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    fn connect_account(&mut self) {
        match (self.cfg.api_id.trim().parse::<i32>(), self.cfg.api_hash.trim().is_empty()) {
            (Ok(id), false) => {
                self.cfg.api_hash = self.cfg.api_hash.trim().to_string();
                self.cfg.save().ok();
                self.account = Some(Account::start(id, self.cfg.api_hash.clone()));
            }
            _ => *self.notice.lock().unwrap() = "Enter a numeric API ID and the API hash from my.telegram.org.".into(),
        }
    }

    /// Settings block: optional account login that can create a chat and add the bot automatically.
    fn account_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Telegram account (optional)");
        ui.label("Log in with your own Telegram account to create a new private channel and add your bot to it automatically.");
        let Some(acc) = self.account.clone() else {
            ui.small("Get an API ID and hash at https://my.telegram.org → API development tools. Only needed for this feature.");
            ui.small("The login session gives full access to your Telegram account. It is stored only on this PC (owner-only folder) and \"Log out\" deletes it.");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.cfg.api_id).hint_text("API ID").desired_width(110.0));
                ui.add(egui::TextEdit::singleline(&mut self.cfg.api_hash).password(true).hint_text("API hash").desired_width(260.0));
                if ui.button("Connect").clicked() {
                    self.connect_account();
                }
            });
            return;
        };
        let (stage, busy, msg) = {
            let g = acc.st.lock().unwrap();
            (g.stage.clone(), g.busy, g.msg.clone())
        };
        match stage {
            Stage::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting to Telegram…");
                });
            }
            Stage::NeedPhone => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.phone).hint_text("+1 555 0100").desired_width(180.0));
                    if ui.add_enabled(!busy && !self.phone.trim().is_empty(), egui::Button::new("Send code")).clicked() {
                        acc.send(account::Cmd::Phone(self.phone.clone()));
                    }
                });
            }
            Stage::NeedCode => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.code).hint_text("login code").desired_width(140.0));
                    if ui.add_enabled(!busy && !self.code.trim().is_empty(), egui::Button::new("Sign in")).clicked() {
                        acc.send(account::Cmd::Code(self.code.clone()));
                        self.code.clear();
                    }
                });
            }
            Stage::NeedPassword(hint) => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text(format!("two-step password {hint}")).desired_width(240.0));
                    if ui.add_enabled(!busy && !self.password.is_empty(), egui::Button::new("Submit")).clicked() {
                        acc.send(account::Cmd::Password(self.password.clone()));
                        self.password.clear();
                    }
                });
            }
            Stage::Ready(name) => {
                ui.label(format!("Signed in as {name}"));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.new_chat_name).hint_text("channel name").desired_width(200.0));
                    let ok = !busy && !self.new_chat_name.trim().is_empty() && !self.primary.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new("✨ Create channel + add bot")).clicked() {
                        let (token, title, acc2) = (self.primary.trim().to_string(), self.new_chat_name.trim().to_string(), acc.clone());
                        acc.st.lock().unwrap().busy = true;
                        *self.notice.lock().unwrap() = "Creating channel…".into();
                        std::thread::spawn(move || {
                            let tg = crate::telegram::Tg::new(&[token], "");
                            match tg.get_me(0) {
                                Ok(u) => acc2.send(account::Cmd::CreateChannel { title, bot_username: u }),
                                Err(e) => {
                                    let mut g = acc2.st.lock().unwrap();
                                    g.busy = false;
                                    g.created = Some(Err(format!("Bot check failed: {e}")));
                                }
                            }
                        });
                    }
                    if ui.button("Log out").on_hover_text("Signs out and deletes the saved session").clicked() {
                        acc.send(account::Cmd::Logout);
                    }
                });
            }
        }
        if busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Working…");
            });
        }
        if !msg.is_empty() {
            ui.label(msg);
        }
    }

    fn running(&self) -> bool {
        self.prog.lock().unwrap().running
    }

    fn start(&self, kind: Kind) {
        if self.running() {
            return;
        }
        self.cancel.store(false, Ordering::Relaxed);
        self.prog.lock().unwrap().running = true;
        let (cfg, store, p, c) = (self.cfg.clone(), self.store.clone(), self.prog.clone(), self.cancel.clone());
        std::thread::spawn(move || match kind {
            Kind::Restore => sync::run_restore(cfg, p, c),
            Kind::Sync => sync::run_sync(cfg, store, p, c, false),
            Kind::Refresh => sync::run_refresh(cfg, store, p),
        });
    }

    /// Files or folders dropped on the window: upload just those to the active chat.
    fn start_upload(&mut self, paths: Vec<std::path::PathBuf>) {
        if !self.cfg.ready() {
            *self.notice.lock().unwrap() = "Set a bot token and add a chat in Settings first.".into();
            return;
        }
        if self.running() {
            *self.notice.lock().unwrap() = "Busy: wait for the current task to finish, then drop again.".into();
            return;
        }
        let mut cfg = self.cfg.clone();
        cfg.folders = paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
        *self.notice.lock().unwrap() = format!("Uploading {} dropped item(s) to \"{}\"…", paths.len(), self.cfg.chat_name());
        self.cancel.store(false, Ordering::Relaxed);
        self.prog.lock().unwrap().running = true;
        let (store, p, c) = (self.store.clone(), self.prog.clone(), self.cancel.clone());
        std::thread::spawn(move || sync::run_sync(cfg, store, p, c, true));
    }

    fn start_delete(&self, items: Vec<Item>) {
        if self.running() {
            return;
        }
        self.prog.lock().unwrap().running = true;
        let (cfg, store, p) = (self.cfg.clone(), self.store.clone(), self.prog.clone());
        std::thread::spawn(move || sync::run_delete(cfg, store, p, items));
    }

    /// One background worker fetches thumbnails for cloud items as they scroll into view.
    fn ensure_thumb_worker(&mut self, ctx: &egui::Context) {
        if self.thumb_tx.is_some() || !self.cfg.ready() {
            return;
        }
        let (tx, rx) = mpsc::channel::<Item>();
        let rx = Arc::new(Mutex::new(rx));
        // 4 workers: thumbnails are small, latency-bound downloads
        for _ in 0..4 {
            let (rx, tg, store, prog, ctx) = (
                rx.clone(),
                crate::telegram::Tg::new(&self.cfg.tokens, &self.cfg.chat_id),
                self.store.clone(),
                self.prog.clone(),
                ctx.clone(),
            );
            std::thread::spawn(move || loop {
                let next = rx.lock().unwrap().recv();
                let Ok(i) = next else { return };
                sync::fetch_thumb(&tg, &i, &store);
                prog.lock().unwrap().version += 1;
                ctx.request_repaint();
            });
        }
        self.thumb_tx = Some(tx);
    }

    fn open_item(&self, i: &Item) {
        if i.status != "CLOUD_ONLY" {
            std::process::Command::new("xdg-open").arg(&i.path).spawn().ok();
            return;
        }
        // unsplit file over 20 MB: a bot cannot fetch it, so open its message in the Telegram app
        if i.chunk_ids.is_empty() && i.size as u64 > crate::telegram::MAX_SINGLE {
            let msg = i.message_id.filter(|m| *m > 0);
            match (self.cfg.chat_id.strip_prefix("-100"), msg) {
                (Some(chan), Some(m)) => {
                    std::process::Command::new("xdg-open").arg(format!("https://t.me/c/{chan}/{m}")).spawn().ok();
                    *self.notice.lock().unwrap() = format!("{} is over 20 MB: opening it in Telegram.", i.name);
                }
                _ => *self.notice.lock().unwrap() = format!("{} is over 20 MB: open it in the Telegram app.", i.name),
            }
            return;
        }
        let (tg, i, n) = (crate::telegram::Tg::new(&self.cfg.tokens, &self.cfg.chat_id), i.clone(), self.notice.clone());
        *n.lock().unwrap() = format!("Downloading {} …", i.name);
        std::thread::spawn(move || match sync::download_for_open(&tg, &i) {
            Ok(p) => {
                *n.lock().unwrap() = String::new();
                std::process::Command::new("xdg-open").arg(p).spawn().ok();
            }
            Err(e) => *n.lock().unwrap() = format!("Open failed: {e}"),
        });
    }

    fn progress_ui(&self, ui: &mut egui::Ui) {
        let g = self.prog.lock().unwrap();
        if g.running {
            let frac = if g.total > 0 { g.done as f32 / g.total as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).text(format!("{}: {}/{}  {}", g.task, g.done, g.total, g.current)));
        }
        let last = g.log.last().cloned().unwrap_or_default();
        egui::CollapsingHeader::new(if g.running || last.is_empty() { "Activity".to_string() } else { format!("Activity: {last}") })
            .id_salt("activity")
            .show(ui, |ui| {
                egui::ScrollArea::vertical().stick_to_bottom(true).max_height(140.0).show(ui, |ui| {
                    for l in &g.log {
                        ui.monospace(l);
                    }
                });
            });
    }

    fn gallery(&mut self, ui: &mut egui::Ui) {
        let mut want: Vec<Item> = vec![];
        let mut open: Option<Item> = None;
        let mut refresh = false;
        let mut delete_req: Option<Vec<Item>> = None;
        let mut rebackup: Option<String> = None;
        let mut switch_to: Option<String> = None;
        ui.horizontal_wrapped(|ui| {
            let running = self.running();
            ui.add_enabled_ui(!running, |ui| {
                egui::ComboBox::from_id_salt("chat_picker").selected_text(format!("💬 {}", self.cfg.chat_name())).show_ui(ui, |ui| {
                    for c in &self.cfg.chats {
                        if ui.selectable_label(c.id == self.cfg.chat_id, &c.name).clicked() {
                            switch_to = Some(c.id.clone());
                        }
                    }
                    ui.separator();
                    if ui.button("➕ Add new chat…").clicked() {
                        self.tab = Tab::Settings;
                    }
                });
            });
            ui.separator();
            ui.selectable_value(&mut self.filter, Filter::All, "All");
            ui.selectable_value(&mut self.filter, Filter::Photos, "Photos");
            ui.selectable_value(&mut self.filter, Filter::Videos, "Videos");
            ui.selectable_value(&mut self.filter, Filter::Files, "Files");
            let cloud = self.items.iter().filter(|i| i.status == "CLOUD_ONLY").count();
            let backed = self.items.iter().filter(|i| i.status == "BACKED_UP").count();
            ui.label(format!("{} items | {} on this PC, backed up | {} in Telegram only", self.items.len(), backed, cloud));
            let running = self.running();
            if ui.add_enabled(self.cfg.ready() && !running, egui::Button::new("⟳ Refresh from Telegram")).clicked() {
                refresh = true;
            }
            if running {
                ui.spinner();
                ui.label(self.prog.lock().unwrap().task.clone());
            }
            ui.separator();
            if ui.add_enabled(self.cfg.ready() && !running, egui::Button::new("⬆ Sync now")).on_hover_text("Upload new files from your backup folders").clicked() {
                self.start(Kind::Sync);
            }
            if ui.add_enabled(self.cfg.ready() && !running, egui::Button::new("⬇ Restore all…")).on_hover_text("Download everything in Telegram to your restore folder").clicked() {
                self.confirm_restore = true;
            }
            if ui.add_enabled(running, egui::Button::new("Cancel")).clicked() {
                self.cancel.store(true, Ordering::Relaxed);
            }
            if ui
                .checkbox(&mut self.cfg.strip_gps, "Strip location")
                .on_hover_text("Remove GPS coordinates from JPEG photos before uploading (off by default). Applies to the next sync or drop.")
                .changed()
            {
                self.cfg.save().ok();
            }
            ui.separator();
            if ui.selectable_label(self.select_mode, "Select").clicked() {
                self.select_mode = !self.select_mode;
                self.selected.clear();
            }
            if self.select_mode {
                let n = self.selected.len();
                if ui.add_enabled(n > 0 && !running, egui::Button::new(format!("🗑 Delete selected ({n})"))).clicked() {
                    let sel: Vec<Item> = self.items.iter().filter(|i| self.selected.contains(&i.id)).cloned().collect();
                    delete_req = Some(sel);
                }
            }
        });
        let n = self.notice.lock().unwrap().clone();
        if !n.is_empty() {
            ui.label(n);
        }
        self.progress_ui(ui);
        ui.separator();
        let shown: Vec<&Item> = self.items.iter().filter(|i| match self.filter {
            Filter::All => true,
            Filter::Photos => !i.video && !i.other,
            Filter::Videos => i.video,
            Filter::Files => i.other,
        }).collect();
        if shown.is_empty() {
            ui.label("Nothing here yet. Drag photos, videos, PDFs or any files or folders here to upload them, press Refresh from Telegram to load an existing backup, or add a folder in Settings and Sync.");
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for i in shown {
                    let size = egui::vec2(150.0, 150.0);
                    // fixed square cell; the photo is centred inside it
                    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                    match &i.thumb {
                        Some(t) => {
                            ui.put(rect, egui::Image::new(format!("file://{t}")).fit_to_exact_size(size));
                        }
                        None => {
                            ui.painter().rect_filled(rect, 4.0, ui.visuals().faint_bg_color);
                            if i.other {
                                let ext = std::path::Path::new(&i.name).extension().and_then(|e| e.to_str()).unwrap_or("file").to_uppercase();
                                ui.painter().text(rect.center() - egui::vec2(0.0, 10.0), egui::Align2::CENTER_CENTER, format!("📄 {ext}"), egui::FontId::proportional(22.0), ui.visuals().text_color());
                                let short: String = i.name.chars().take(22).collect();
                                ui.painter().text(rect.center() + egui::vec2(0.0, 22.0), egui::Align2::CENTER_CENTER, short, egui::FontId::proportional(11.0), ui.visuals().weak_text_color());
                            } else {
                                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER,
                                    if i.video { "▶ video" } else { "image" }, egui::FontId::proportional(16.0), ui.visuals().text_color());
                            }
                            if i.status == "CLOUD_ONLY" && !i.other && ui.is_rect_visible(rect) {
                                want.push(i.clone());
                            }
                        }
                    }
                    let badge = match i.status.as_str() {
                        "BACKED_UP" => "✔",
                        "CLOUD_ONLY" => "☁",
                        "FAILED" => "✖",
                        "EXCLUDED" => "⊘",
                        _ => "…",
                    };
                    let resp = resp.on_hover_text(format!("{}\n{} {}", i.name, badge, i.error.clone().unwrap_or_default()));
                    ui.painter().text(resp.rect.right_top() + egui::vec2(-12.0, 12.0), egui::Align2::CENTER_CENTER, badge,
                        egui::FontId::proportional(18.0), egui::Color32::WHITE);
                    if self.selected.contains(&i.id) {
                        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(3.0, egui::Color32::LIGHT_BLUE), egui::StrokeKind::Inside);
                    }
                    if resp.clicked() {
                        if self.select_mode {
                            if !self.selected.remove(&i.id) {
                                self.selected.insert(i.id.clone());
                            }
                        } else {
                            open = Some(i.clone());
                        }
                    }
                    resp.context_menu(|ui| {
                        if matches!(i.status.as_str(), "BACKED_UP" | "CLOUD_ONLY") && ui.button("🗑 Delete from Telegram backup…").clicked() {
                            delete_req = Some(vec![i.clone()]);
                            ui.close();
                        }
                        if matches!(i.status.as_str(), "EXCLUDED" | "FAILED") && ui.button("Back up again").clicked() {
                            rebackup = Some(i.id.clone());
                            ui.close();
                        }
                    });
                }
            });
        });
        if let Some(id) = switch_to {
            if id != self.cfg.chat_id {
                self.activate(&id);
            }
        }
        if refresh {
            self.start(Kind::Refresh);
        }
        if let Some(id) = rebackup {
            self.store.set_status(&id, "PENDING");
            self.items = self.store.all();
        }
        if delete_req.is_some() {
            self.confirm_delete = delete_req;
        }
        if let Some(i) = open {
            self.open_item(&i);
        }
        if let Some(tx) = &self.thumb_tx {
            for i in want {
                if self.requested.insert(i.id.clone()) {
                    tx.send(i).ok();
                }
            }
        }
    }

    /// Rebuild cfg.tokens as [primary] + extras, dropping blanks.
    fn set_tokens(&mut self, extras: Vec<String>) {
        let mut t = vec![self.primary.trim().to_string()];
        t.extend(extras);
        self.cfg.tokens = t.into_iter().filter(|x| !x.is_empty()).collect();
        self.thumb_tx = None; // credentials changed
        self.requested.clear();
    }

    fn extras(&self) -> Vec<String> {
        self.cfg.tokens.iter().skip(1).cloned().collect()
    }

    fn save_notice(&self) {
        *self.notice.lock().unwrap() = match self.cfg.save() {
            Ok(()) => "Saved".into(),
            Err(e) => format!("Save failed: {e}"),
        };
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Appearance");
            ui.horizontal(|ui| {
                let before = self.cfg.theme.clone();
                for (v, l) in [("system", "System"), ("light", "Light"), ("dark", "Dark")] {
                    let on = self.cfg.theme == v || (self.cfg.theme.is_empty() && v == "system");
                    if ui.selectable_label(on, l).clicked() {
                        self.cfg.theme = v.into();
                    }
                }
                if self.cfg.theme != before {
                    apply_theme(ui.ctx(), &self.cfg.theme);
                    self.cfg.save().ok();
                }
            });
            ui.separator();

            ui.heading("Account");
            ui.label("Bot token");
            ui.add(egui::TextEdit::singleline(&mut self.primary).password(true).desired_width(420.0));
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    let extras = self.extras();
                    self.set_tokens(extras);
                    self.save_notice();
                }
                if ui.button("Verify connection").clicked() && self.cfg.ready() {
                    let (toks, chat, n) = (self.cfg.tokens.clone(), self.cfg.chat_id.clone(), self.notice.clone());
                    *n.lock().unwrap() = "Verifying…".into();
                    std::thread::spawn(move || {
                        let tg = crate::telegram::Tg::new(&toks, &chat);
                        let parts: Vec<String> = (0..tg.bots())
                            .map(|b| match tg.get_me(b) {
                                Ok(u) => format!("bot {}: @{u}", b + 1),
                                Err(e) => format!("bot {}: failed ({e})", b + 1),
                            })
                            .collect();
                        let chat_ok = match tg.pinned_document(0) {
                            Ok(_) => "chat reachable".to_string(),
                            Err(e) => format!("chat: {e}"),
                        };
                        *n.lock().unwrap() = format!("{} | {chat_ok}", parts.join(", "));
                    });
                }
                if ui.add(egui::Button::new("Disconnect bot").fill(egui::Color32::from_rgb(140, 30, 30))).clicked() {
                    self.primary.clear();
                    self.cfg.tokens.clear();
                    self.thumb_tx = None;
                    self.save_notice();
                }
            });
            ui.label(self.notice.lock().unwrap().clone());
            ui.separator();

            ui.heading("Chats");
            ui.label("Each chat is its own gallery. Switch between them from the gallery header.");
            let mut use_id: Option<String> = None;
            let mut remove_id: Option<String> = None;
            for c in &self.cfg.chats {
                ui.horizontal(|ui| {
                    let active = c.id == self.cfg.chat_id;
                    ui.label(format!("{} {}  ({})", if active { "●" } else { "○" }, c.name, c.id));
                    if !active && ui.small_button("Use").clicked() {
                        use_id = Some(c.id.clone());
                    }
                    if ui.small_button("remove").on_hover_text("Forget this chat here. Nothing is deleted from Telegram.").clicked() {
                        remove_id = Some(c.id.clone());
                    }
                });
            }
            if let Some(id) = remove_id {
                self.cfg.chats.retain(|c| c.id != id);
                self.store.delete_chat(&id);
                if self.cfg.chat_id == id {
                    let next = self.cfg.chats.first().map(|c| c.id.clone()).unwrap_or_default();
                    self.activate(&next);
                } else {
                    self.cfg.save().ok();
                }
            }
            if let Some(id) = use_id {
                self.activate(&id);
            }
            if ui.button("✨ Create new chat…").clicked() {
                self.start_wizard();
            }
            ui.label("Or add an existing chat");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.add_id).hint_text("chat ID, e.g. -100123…").desired_width(220.0));
                ui.add(egui::TextEdit::singleline(&mut self.add_name).hint_text("name (optional)").desired_width(160.0));
                if ui.button("Add chat").clicked() && !self.add_id.trim().is_empty() && !self.primary.trim().is_empty() {
                    let (id, name) = (self.add_id.trim().to_string(), self.add_name.trim().to_string());
                    let mut toks = vec![self.primary.trim().to_string()];
                    toks.extend(self.extras());
                    let (out, n) = (self.pending_chat.clone(), self.notice.clone());
                    *n.lock().unwrap() = "Checking chat…".into();
                    std::thread::spawn(move || {
                        let tg = crate::telegram::Tg::new(&toks, &id);
                        *out.lock().unwrap() = Some(match tg.chat_title(0) {
                            Ok(title) => Ok(SavedChat { id, name: if name.is_empty() { title } else { name } }),
                            Err(e) => Err(format!("Cannot use that chat: {e} (is the bot an admin there?)")),
                        });
                    });
                }
                if ui.button("Find my chats").clicked() && !self.primary.trim().is_empty() {
                    let (toks, chats, n) = (vec![self.primary.trim().to_string()], self.chats.clone(), self.notice.clone());
                    *n.lock().unwrap() = "Looking for chats…".into();
                    std::thread::spawn(move || {
                        let tg = crate::telegram::Tg::new(&toks, "");
                        match tg.recent_chats(0) {
                            Ok(c) if c.is_empty() => *n.lock().unwrap() = "No chats found. Add the bot as admin to your channel/group and post a message there, then try again.".into(),
                            Ok(c) => {
                                *n.lock().unwrap() = format!("Found {} chat(s) - press Pick", c.len());
                                *chats.lock().unwrap() = c;
                            }
                            Err(e) => *n.lock().unwrap() = format!("Failed: {e}"),
                        }
                    });
                }
            });
            let found = self.chats.lock().unwrap().clone();
            for (id, title, kind) in found {
                ui.horizontal(|ui| {
                    ui.label(format!("{title} ({kind}) {id}"));
                    if ui.small_button("Pick").clicked() {
                        self.add_id = id.to_string();
                        self.add_name = title.clone();
                    }
                });
            }
            ui.small("Telegram bots cannot create chats. In Telegram, create a private channel or group, add your bot as admin, post a message, then Find my chats > Pick > Add chat.");
            ui.separator();

            self.account_ui(ui);
            ui.separator();

            ui.heading("Bot pool");
            ui.label("Extra bots spread uploads across tokens and reduce rate-limit risk. All bots must be admins in the same chat as the primary bot.");
            let extras = self.extras();
            if extras.is_empty() {
                ui.label("No secondary bots. Uploads use the primary bot only.");
            }
            let mut remove: Option<usize> = None;
            for (n, t) in extras.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("Bot {}: {}…{}", n + 2, t.chars().take(4).collect::<String>(), t.chars().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect::<String>()));
                    if ui.small_button("remove").clicked() {
                        remove = Some(n);
                    }
                });
            }
            if let Some(n) = remove {
                let mut e = extras.clone();
                e.remove(n);
                self.set_tokens(e);
                self.save_notice();
            }
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.new_bot).password(true).hint_text("new bot token").desired_width(320.0));
                if ui.button("Add bot").clicked() && !self.new_bot.trim().is_empty() {
                    let mut e = self.extras();
                    e.push(self.new_bot.trim().to_string());
                    self.new_bot.clear();
                    self.set_tokens(e);
                    self.save_notice();
                }
            });
            ui.separator();

            ui.heading("Backup");
            ui.label("Folders to back up:");
            let mut rm = None;
            for (n, f) in self.cfg.folders.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(f);
                    if ui.small_button("remove").clicked() {
                        rm = Some(n);
                    }
                });
            }
            if let Some(n) = rm {
                self.cfg.folders.remove(n);
                self.cfg.save().ok();
            }
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.folder_input).hint_text("/path/to/folder").desired_width(320.0));
                if ui.button("Add folder").clicked() && std::path::Path::new(self.folder_input.trim()).is_dir() {
                    self.cfg.folders.push(self.folder_input.trim().to_string());
                    self.folder_input.clear();
                    self.cfg.save().ok();
                }
            });
            if ui.checkbox(&mut self.cfg.strip_gps, "Strip location from photos").changed() {
                self.cfg.save().ok();
            }
            ui.small("Removes GPS coordinates from JPEG photos before uploading. Rotation and other info are kept. Applies to new uploads only.");
            ui.label("Upload speed");
            ui.horizontal(|ui| {
                let before = self.cfg.conservative;
                ui.selectable_value(&mut self.cfg.conservative, false, "Balanced");
                ui.selectable_value(&mut self.cfg.conservative, true, "Conservative");
                if before != self.cfg.conservative {
                    self.cfg.save().ok();
                }
            });
            ui.small("Conservative pauses 3s between files: slower, extra protection against account limits.");
            ui.horizontal(|ui| {
                ui.label("Restore folder");
                ui.text_edit_singleline(&mut self.cfg.restore_dir);
                if ui.button("Save").clicked() {
                    self.cfg.save().ok();
                }
            });
            ui.separator();

            ui.heading("Storage");
            let sum = |f: &dyn Fn(&Item) -> bool| self.items.iter().filter(|i| f(i)).map(|i| i.size.max(0) as u64).sum::<u64>();
            let backed = sum(&|i| i.status == "BACKED_UP" || i.status == "CLOUD_ONLY");
            let pending = sum(&|i| i.status == "PENDING" || i.status == "FAILED");
            let excluded = self.items.iter().filter(|i| i.status == "EXCLUDED").count();
            let freeable = sum(&|i| i.status == "BACKED_UP" && !i.path.is_empty() && std::path::Path::new(&i.path).exists());
            ui.label(format!("Backed up: {}", human(backed)));
            ui.label(format!("Pending upload: {}", human(pending)));
            if excluded > 0 {
                ui.label(format!("{excluded} file(s) excluded (deleted from Telegram, kept on this PC)"));
            }
            if ui.add_enabled(freeable > 0 && !self.running(), egui::Button::new(format!("Remove local copies ({})…", human(freeable)))).clicked() {
                self.confirm_free = true;
            }
            ui.small("Deletes files on this PC that are already safely in Telegram. They stay in the gallery as cloud items.");
            ui.separator();

            ui.heading("About");
            ui.label(format!("Ulap for Linux {}", env!("CARGO_PKG_VERSION")));
            ui.label("Ulap backs up your media directly to your Telegram account. No servers, no subscriptions.");
        });
    }

    fn free_local_copies(&self) {
        let todo: Vec<Item> = self
            .items
            .iter()
            .filter(|i| i.status == "BACKED_UP" && i.file_id.is_some() && !i.path.is_empty())
            .cloned()
            .collect();
        let (store, prog, n) = (self.store.clone(), self.prog.clone(), self.notice.clone());
        std::thread::spawn(move || {
            let (mut done, mut bytes) = (0, 0u64);
            for i in todo {
                match std::fs::remove_file(&i.path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => continue,
                }
                store.drop_local(&i.id);
                done += 1;
                bytes += i.size.max(0) as u64;
            }
            prog.lock().unwrap().version += 1;
            *n.lock().unwrap() = format!("Removed {done} local file(s), freed {}", human(bytes));
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if self.confirm_free {
            let mut decision: Option<bool> = None;
            #[allow(deprecated)]
            let modal = egui::Modal::new(egui::Id::new("confirm_free"));
            modal.show(ctx, |ui| {
                ui.heading("Remove local copies?");
                ui.label("Files on this PC that are backed up in Telegram will be permanently deleted from disk. They remain in your Telegram chat and in the gallery.");
                ui.horizontal(|ui| {
                    if ui.button("Remove").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
            if let Some(yes) = decision {
                self.confirm_free = false;
                if yes {
                    self.free_local_copies();
                }
            }
            return;
        }
        if self.confirm_restore {
            let mut decision: Option<bool> = None;
            let cloud = self.items.iter().filter(|i| i.status == "CLOUD_ONLY").count();
            #[allow(deprecated)]
            let modal = egui::Modal::new(egui::Id::new("confirm_restore"));
            modal.show(ctx, |ui| {
                ui.heading("Restore from Telegram?");
                ui.label(format!("Downloads up to {cloud} file(s) into {}. Files already there are skipped.", self.cfg.restore_dir));
                ui.horizontal(|ui| {
                    if ui.button("Restore").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
            if let Some(yes) = decision {
                self.confirm_restore = false;
                if yes {
                    self.start(Kind::Restore);
                }
            }
            return;
        }
        let Some(items) = self.confirm_delete.clone() else { return };
        let on_pc = items.iter().filter(|i| !i.path.is_empty()).count();
        let mut decision: Option<bool> = None;
        #[allow(deprecated)]
        let modal = egui::Modal::new(egui::Id::new("confirm_delete"));
        modal.show(ctx, |ui| {
            ui.heading(format!("Delete {} item(s) from Telegram?", items.len()));
            ui.label("This removes them from your Telegram chat and from the backup index. It cannot be undone.");
            if on_pc > 0 {
                ui.label(format!("{on_pc} of them are also on this PC: those files are kept and will not be uploaded again."));
            }
            ui.horizontal(|ui| {
                if ui.button("Delete").clicked() {
                    decision = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
            });
        });
        if let Some(yes) = decision {
            self.confirm_delete = None;
            if yes {
                self.selected.clear();
                self.select_mode = false;
                self.start_delete(items);
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // background threads (verify, wizard, chat check) report through shared state: redraw twice a second
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
        if !self.started_account {
            self.started_account = true;
            // stay logged in across restarts when a session exists
            if self.account.is_none() && account::session_path().exists() && self.cfg.api_id.trim().parse::<i32>().is_ok() && !self.cfg.api_hash.is_empty() {
                self.connect_account();
            }
        }
        if let Some(acc) = &self.account {
            let (created, gone) = {
                let mut g = acc.st.lock().unwrap();
                (g.created.take(), g.logged_out)
            };
            match created {
                Some(Ok((id, title))) => {
                    self.auto_activate = true;
                    *self.pending_chat.lock().unwrap() = Some(Ok(SavedChat { id, name: title }));
                }
                Some(Err(e)) => *self.notice.lock().unwrap() = e,
                None => {}
            }
            if gone {
                self.account = None;
            }
        }
        let added = self.pending_chat.lock().unwrap().take();
        match added {
            Some(Ok(c)) => {
                if !self.cfg.chats.iter().any(|x| x.id == c.id) {
                    self.cfg.chats.push(c.clone());
                }
                self.add_id.clear();
                self.add_name.clear();
                *self.notice.lock().unwrap() = format!("Added chat \"{}\"", c.name);
                if self.wizard.take().is_some() || std::mem::take(&mut self.auto_activate) || self.cfg.chat_id.is_empty() || !self.cfg.ready() {
                    self.activate(&c.id);
                } else {
                    self.cfg.save().ok();
                }
            }
            Some(Err(e)) => {
                self.wizard = None;
                *self.notice.lock().unwrap() = e;
            }
            None => {}
        }
        #[allow(deprecated)]
        let overlay_id = egui::Id::new("drop_overlay");
        if self.tab == Tab::Gallery {
            let dropped: Vec<std::path::PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
            if !dropped.is_empty() {
                self.start_upload(dropped);
            }
            if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
                let screen = ctx.content_rect();
                let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, overlay_id));
                painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(170));
                painter.rect_stroke(screen.shrink(12.0), 8.0, egui::Stroke::new(3.0, egui::Color32::LIGHT_BLUE), egui::StrokeKind::Inside);
                painter.text(
                    screen.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("Drop to upload to \"{}\"", self.cfg.chat_name()),
                    egui::FontId::proportional(26.0),
                    egui::Color32::WHITE,
                );
            }
        }
        self.ensure_thumb_worker(&ctx);
        if !self.started {
            self.started = true;
            apply_theme(&ctx, &self.cfg.theme);
            if self.cfg.ready() {
                self.start(Kind::Refresh); // show the existing Telegram gallery on launch
            }
        }
        let v = self.prog.lock().unwrap().version;
        if v != self.seen_version {
            self.seen_version = v;
            self.items = self.store.all();
        }
        if self.running() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        egui::Panel::left("nav").exact_size(120.0).show(ui, |ui| {
            ui.heading("Ulap");
            ui.selectable_value(&mut self.tab, Tab::Gallery, "Gallery");
            ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
        });
        self.confirm_dialog(&ctx);
        self.wizard_dialog(&ctx);
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Gallery => self.gallery(ui),
            Tab::Settings => self.settings(ui),
        });
    }
}

fn human(b: u64) -> String {
    let (mut v, mut u) = (b as f64, 0);
    while v >= 1024.0 && u < 4 {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.1} {}", v, ["B", "KB", "MB", "GB", "TB"][u])
}

fn apply_theme(ctx: &egui::Context, theme: &str) {
    ctx.set_theme(match theme {
        "light" => egui::ThemePreference::Light,
        "dark" => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_sizes() {
        assert_eq!(super::human(0), "0.0 B");
        assert_eq!(super::human(1536), "1.5 KB");
        assert_eq!(super::human(5 * 1024 * 1024 * 1024), "5.0 GB");
    }
}
