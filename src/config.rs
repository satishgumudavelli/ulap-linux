use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
pub struct SavedChat {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)] // older config files lack newer fields; never lose the saved tokens
pub struct Config {
    /// One bot token per entry; extra bots spread upload load (round-robin).
    pub tokens: Vec<String>,
    /// the active chat (gallery, uploads and restore all use it)
    pub chat_id: String,
    pub chats: Vec<SavedChat>,
    pub folders: Vec<String>,
    pub restore_dir: String,
    /// optional Telegram account login (my.telegram.org); only used to create chats
    pub api_id: String,
    pub api_hash: String,
    /// "system" | "light" | "dark"
    pub theme: String,
    /// remove GPS coordinates from JPEGs before uploading
    pub strip_gps: bool,
    /// pause between uploads to stay clear of account limits
    pub conservative: bool,
}

pub fn dirs() -> ProjectDirs {
    ProjectDirs::from("com", "ulap", "ulap").expect("no home directory")
}

pub fn data_dir() -> PathBuf {
    let d = dirs().data_dir().to_path_buf();
    std::fs::create_dir_all(&d).ok();
    d
}

fn path() -> PathBuf {
    let d = dirs().config_dir().to_path_buf();
    std::fs::create_dir_all(&d).ok();
    d.join("config.json")
}

impl Config {
    pub fn load() -> Config {
        let mut c: Config = std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let id = c.chat_id.trim().to_string();
        if !id.is_empty() && !c.chats.iter().any(|x| x.id == id) {
            c.chats.push(SavedChat { id: id.clone(), name: "Main chat".into() }); // pre-multi-chat configs
        }
        if c.restore_dir.is_empty() {
            c.restore_dir = dirs::home().join("Ulap-Restore").to_string_lossy().into();
        }
        c
    }

    pub fn save(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let p = path();
        std::fs::write(&p, serde_json::to_string_pretty(self).unwrap())?;
        // holds bot tokens: owner-only
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))
    }

    pub fn chat_name(&self) -> String {
        self.chats.iter().find(|c| c.id == self.chat_id).map(|c| c.name.clone()).unwrap_or_else(|| "No chat".into())
    }

    pub fn ready(&self) -> bool {
        !self.tokens.is_empty() && !self.chat_id.trim().is_empty()
    }
}

mod dirs {
    pub fn home() -> std::path::PathBuf {
        directories::UserDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .unwrap_or_else(|| ".".into())
    }
}
