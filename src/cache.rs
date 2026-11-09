//! Tool lists persisted across restarts, so servers do not have to be started just to
//! learn their tools. One JSON file per server in the platform cache directory.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rmcp::model::Tool;
use serde::{Deserialize, Serialize};

/// Bumped when the file layout changes; older files are ignored.
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Entry {
    version: u32,
    fingerprint: String,
    instructions: Option<String>,
    /// Unfiltered, so changing `include_tools`/`exclude_tools` needs no refresh.
    tools: Vec<Tool>,
}

pub struct CachedTools {
    pub tools: Vec<Tool>,
    pub instructions: Option<String>,
}

#[derive(Clone)]
pub struct ToolStore {
    dir: Option<PathBuf>,
}

impl ToolStore {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    /// The entry for `server`, if it exists and was written for the same `fingerprint`.
    pub fn load(&self, server: &str, fingerprint: &str) -> Option<CachedTools> {
        let path = self.path(server)?;
        let raw = std::fs::read(&path).ok()?;
        let entry: Entry = match serde_json::from_slice(&raw) {
            Ok(entry) => entry,
            Err(e) => {
                tracing::debug!(server, error = %e, "ignoring unreadable tool cache");
                return None;
            }
        };
        (entry.version == FORMAT_VERSION && entry.fingerprint == fingerprint).then_some(
            CachedTools {
                tools: entry.tools,
                instructions: entry.instructions,
            },
        )
    }

    /// Best effort: a failing write only costs a server start on the next launch.
    pub fn save(
        &self,
        server: &str,
        fingerprint: &str,
        tools: &[Tool],
        instructions: Option<&str>,
    ) {
        let Some(path) = self.path(server) else {
            return;
        };
        let entry = Entry {
            version: FORMAT_VERSION,
            fingerprint: fingerprint.to_owned(),
            instructions: instructions.map(str::to_owned),
            tools: tools.to_vec(),
        };
        if let Err(e) = write_atomic(&path, &entry) {
            tracing::warn!(server, path = %path.display(), "cannot write tool cache: {e:#}");
        }
    }

    fn path(&self, server: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{server}.json")))
    }
}

/// Write to a temporary file and rename, so a crash never leaves a truncated entry.
fn write_atomic(path: &Path, entry: &Entry) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}-{unique}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(entry)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Stable 64-bit FNV-1a; unlike `DefaultHasher` it does not change between Rust releases.
pub fn fingerprint(data: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use rmcp::model::JsonObject;

    use super::*;

    fn temp_store() -> (ToolStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "emcipi-cache-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (ToolStore::new(Some(dir.clone())), dir)
    }

    #[test]
    fn roundtrip_and_invalidation() {
        let (store, dir) = temp_store();
        let tools = vec![Tool::new("add", "Add numbers", JsonObject::new())];
        assert!(store.load("math", "fp1").is_none());

        store.save("math", "fp1", &tools, Some("Use add."));
        let cached = store.load("math", "fp1").unwrap();
        assert_eq!(cached.tools, tools);
        assert_eq!(cached.instructions.as_deref(), Some("Use add."));
        assert!(store.load("math", "fp2").is_none(), "other fingerprint");
        assert!(store.load("other", "fp1").is_none());

        std::fs::write(dir.join("math.json"), b"{broken").unwrap();
        assert!(store.load("math", "fp1").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disabled_store_does_nothing() {
        let store = ToolStore::new(None);
        store.save("x", "fp", &[], None);
        assert!(store.load("x", "fp").is_none());
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint(b""), "cbf29ce484222325");
        assert_eq!(fingerprint(b"a"), "af63dc4c8601ec8c");
        assert_ne!(fingerprint(b"ab"), fingerprint(b"ba"));
    }
}
