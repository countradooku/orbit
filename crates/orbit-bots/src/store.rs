//! What the bots remember: `<data>/bots.bin`.
//!
//! Which platforms are on, the chat options, who is paired (and the chat
//! they paired from, where needs-you pushes go) and which chat drives which
//! thread. No tokens: those are the engine's, in its integrations file.
//! Written atomically and readable by the user only (it names people).

use std::path::{Path, PathBuf};

use bitcode::{Decode, Encode};
use orbit_proto::*;

const FILE: &str = "bots.bin";
const VERSION: u8 = 1;

/// A conversation: a chat, and in Slack a thread within it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ChatKey {
    pub platform: Platform,
    /// Telegram chat id, Slack channel id or Discord channel id.
    pub chat: String,
    /// Slack thread timestamp; empty elsewhere and for a Slack DM.
    pub thread: String,
}

impl ChatKey {
    pub fn new(platform: Platform, chat: impl Into<String>) -> Self {
        Self { platform, chat: chat.into(), thread: String::new() }
    }

    pub fn in_thread(mut self, thread: impl Into<String>) -> Self {
        self.thread = thread.into();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Paired {
    pub user: PairedUser,
    /// Where they paired from: their direct chat with the bot.
    pub home: ChatKey,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Stored {
    pub enabled: Vec<Platform>,
    pub options: BotOptions,
    pub paired: Vec<Paired>,
    /// Which thread each chat drives.
    pub bindings: Vec<(ChatKey, ThreadId)>,
}

impl Stored {
    pub fn enabled(&self, platform: Platform) -> bool {
        self.enabled.contains(&platform)
    }

    pub fn set_enabled(&mut self, platform: Platform, on: bool) {
        self.enabled.retain(|p| *p != platform);
        if on {
            self.enabled.push(platform);
            self.enabled.sort();
        }
    }

    pub fn paired(&self, platform: Platform, user_id: &str) -> Option<&Paired> {
        self.paired.iter().find(|p| p.user.platform == platform && p.user.user_id == user_id)
    }

    pub fn binding(&self, chat: &ChatKey) -> Option<ThreadId> {
        self.bindings.iter().find(|(c, _)| c == chat).map(|(_, t)| *t)
    }

    pub fn bind(&mut self, chat: ChatKey, thread: ThreadId) {
        self.bindings.retain(|(c, _)| *c != chat);
        self.bindings.push((chat, thread));
    }

    /// Chats driving `thread`.
    pub fn chats_of(&self, thread: ThreadId) -> impl Iterator<Item = &ChatKey> {
        self.bindings.iter().filter(move |(_, t)| *t == thread).map(|(c, _)| c)
    }
}

pub struct Store {
    path: PathBuf,
    pub data: Stored,
}

impl Store {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(FILE);
        let data = match std::fs::read(&path) {
            Ok(bytes) => match bytes.split_first() {
                Some((&VERSION, rest)) => bitcode::decode(rest).unwrap_or_else(|e| {
                    log::warn!("{}: unreadable ({e}); starting empty", path.display());
                    Stored::default()
                }),
                _ => Stored::default(),
            },
            Err(_) => Stored::default(),
        };
        Self { path, data }
    }

    pub fn save(&self) {
        if let Err(err) = write(&self.path, &self.data) {
            log::warn!("{}: couldn’t save ({err})", self.path.display());
        }
    }
}

fn write(path: &Path, data: &Stored) -> std::io::Result<()> {
    let mut bytes = vec![VERSION];
    bytes.extend(bitcode::encode(data));
    let tmp = path.with_extension("bin.tmp");
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path());
        assert_eq!(store.data, Stored::default());
        store.data.set_enabled(Platform::Slack, true);
        let chat = ChatKey::new(Platform::Slack, "C1").in_thread("17.1");
        store.data.bind(chat.clone(), ThreadId(9));
        store.data.paired.push(Paired {
            user: PairedUser { platform: Platform::Slack, user_id: "U1".into(), name: "Ana".into(), paired_ms: 1 },
            home: ChatKey::new(Platform::Slack, "D1"),
        });
        store.save();
        let again = Store::open(dir.path());
        assert_eq!(again.data, store.data);
        assert_eq!(again.data.binding(&chat), Some(ThreadId(9)));
        assert!(again.data.enabled(Platform::Slack) && !again.data.enabled(Platform::Discord));
    }
}
