use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use wotoha_contracts::{ChannelKey, GuildKey};

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_STATE_FILE: &str = ".wotoha-reconnect.json";

#[derive(Clone)]
pub(crate) struct ReconnectStore {
    path: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct ReconnectState {
    schema_version: u32,
    connections: Vec<StoredConnection>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct StoredConnection {
    guild_id: u64,
    channel_id: u64,
}

impl ReconnectStore {
    pub(crate) fn from_env() -> Self {
        let path = std::env::var_os("WOTOHA_RECONNECT_STATE_FILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_FILE));
        Self { path }
    }

    #[cfg(test)]
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn save(&self, connections: &[(GuildKey, ChannelKey)]) -> io::Result<()> {
        if connections.is_empty() {
            return remove_if_exists(&self.path);
        }

        if let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let state = ReconnectState {
            schema_version: SCHEMA_VERSION,
            connections: connections
                .iter()
                .map(|(guild_id, channel_id)| StoredConnection {
                    guild_id: guild_id.get(),
                    channel_id: channel_id.get(),
                })
                .collect(),
        };
        let payload = serde_json::to_vec(&state).map_err(io::Error::other)?;
        let temporary = temporary_path(&self.path);
        let write_result = (|| {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(&payload)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    /// Reads and removes the handoff before reconnecting so an ordinary disconnect later in this
    /// process cannot be resurrected by a stale file after a crash.
    pub(crate) fn take(&self) -> io::Result<Vec<(GuildKey, ChannelKey)>> {
        let payload = match fs::read(&self.path) {
            Ok(payload) => payload,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        remove_if_exists(&self.path)?;

        let state: ReconnectState = serde_json::from_slice(&payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if state.schema_version != SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported reconnect state schema {}",
                    state.schema_version
                ),
            ));
        }

        Ok(state
            .connections
            .into_iter()
            .filter(|entry| entry.guild_id != 0 && entry.channel_id != 0)
            .map(|entry| {
                (
                    GuildKey::new(entry.guild_id),
                    ChannelKey::new(entry.channel_id),
                )
            })
            .collect())
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(DEFAULT_STATE_FILE);
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wotoha-reconnect-{name}-{}-{}.json",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed),
        ))
    }

    #[test]
    fn save_then_take_is_one_shot() {
        let path = test_path("one-shot");
        let store = ReconnectStore::new(path.clone());
        let expected = vec![
            (GuildKey::new(10), ChannelKey::new(20)),
            (GuildKey::new(30), ChannelKey::new(40)),
        ];

        store.save(&expected).unwrap();
        assert_eq!(store.take().unwrap(), expected);
        assert!(store.take().unwrap().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn empty_save_removes_an_old_handoff() {
        let path = test_path("empty");
        let store = ReconnectStore::new(path.clone());
        store
            .save(&[(GuildKey::new(10), ChannelKey::new(20))])
            .unwrap();

        store.save(&[]).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn malformed_handoff_is_consumed() {
        let path = test_path("malformed");
        fs::write(&path, b"not json").unwrap();
        let store = ReconnectStore::new(path.clone());

        assert_eq!(store.take().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(!path.exists());
    }
}
