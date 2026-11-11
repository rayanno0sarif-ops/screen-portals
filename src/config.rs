//! Files on disk: this computer's identity key and the list of paired computers.

use crate::layout::{Placement, PortalMode, Side};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// A computer's public key: its permanent identity.
pub type Key = [u8; 32];

/// Bytes as lowercase hex, the form keys take in settings files.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The reverse of `to_hex`; `None` for anything that is not clean hex.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// Short, human-checkable form of a public key.
pub fn fingerprint(key: &Key) -> String {
    to_hex(&key[..4])
}

/// One paired computer, as remembered between runs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerInfo {
    /// The paired computer's public key, in hex.
    pub key: String,
    pub name: String,
    /// Which edge of this computer's desktop leads to it.
    pub side: Side,
    /// How far along that edge it sits, in pixels (see `layout::Placement`).
    #[serde(default)]
    pub offset: i32,
    /// Which part of the edge lets the cursor through.
    #[serde(default)]
    pub mode: PortalMode,
    /// Its desktop size when last seen, so it can be drawn while offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<[u32; 2]>,
    /// Optional fixed address ("host:port") for networks where automatic
    /// discovery is blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
}

impl PeerInfo {
    /// A fresh pairing with the default arrangement: the whole edge on `side`.
    pub fn new(key: &Key, name: &str, side: Side) -> PeerInfo {
        PeerInfo { key: to_hex(key), name: name.to_string(), side, offset: 0, mode: PortalMode::Edge, screen: None, addr: None }
    }

    /// The key as bytes, or `None` if the settings file was edited badly.
    pub fn key_bytes(&self) -> Option<Key> {
        from_hex(&self.key)?.try_into().ok()
    }

    /// Where this computer sits relative to ours.
    pub fn placement(&self) -> Placement {
        Placement { side: self.side, offset: self.offset, mode: self.mode }
    }
}

/// Everything remembered between runs apart from the identity key.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub peers: Vec<PeerInfo>,
    /// Public key (hex) of the computer whose speakers play everyone's
    /// sound. Absent means each computer plays its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speakers: Option<String>,
    /// How many times that choice has been changed; the highest wins when
    /// two computers disagree.
    #[serde(default)]
    pub speakers_rev: u64,
}

impl Config {
    /// The pairing with this key, if there is one.
    pub fn find(&self, key: &Key) -> Option<&PeerInfo> {
        let hex = to_hex(key);
        self.peers.iter().find(|p| p.key == hex)
    }

    /// Add a pairing, or replace the one with the same key in place.
    pub fn upsert(&mut self, peer: PeerInfo) {
        match self.peers.iter_mut().find(|p| p.key == peer.key) {
            Some(existing) => *existing = peer,
            None => self.peers.push(peer),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    private: String,
    public: String,
}

/// This computer's own key pair.
#[derive(Clone)]
pub struct Identity {
    pub private: Vec<u8>,
    pub public: Key,
}

/// The folder holding `identity.json` and `config.json`.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Open (creating if needed) the given folder, or the per-user default.
    pub fn open(dir: Option<PathBuf>) -> Result<Store, String> {
        let dir = match dir {
            Some(dir) => dir,
            None => default_dir()?,
        };
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        Ok(Store { dir })
    }

    /// The folder this store reads and writes.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Read the settings; a missing file means nothing is paired yet.
    pub fn load_config(&self) -> Result<Config, String> {
        let path = self.dir.join("config.json");
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{} is not valid: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Write the settings, replacing the previous file in one step.
    pub fn save_config(&self, config: &Config) -> Result<(), String> {
        let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
        write_atomically(&self.dir.join("config.json"), &text)
    }

    /// Load this computer's key pair, creating it on first use.
    pub fn identity(&self) -> Result<Identity, String> {
        let path = self.dir.join("identity.json");
        match fs::read_to_string(&path) {
            Ok(text) => {
                let file: IdentityFile =
                    serde_json::from_str(&text).map_err(|e| format!("{} is not valid: {e}", path.display()))?;
                let private = from_hex(&file.private).ok_or("identity file has a malformed private key")?;
                let public: Key = from_hex(&file.public)
                    .and_then(|v| v.try_into().ok())
                    .ok_or("identity file has a malformed public key")?;
                Ok(Identity { private, public })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (private, public) = crate::secure::generate_keypair()?;
                let file = IdentityFile { private: to_hex(&private), public: to_hex(&public) };
                write_atomically(&path, &serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?)?;
                Ok(Identity { private, public })
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }
}

/// Write to a temporary file first so a crash never leaves half a file.
fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn default_dir() -> Result<PathBuf, String> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Ok(PathBuf::from(appdata).join("ScreenPortals"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".config").join("screen-portals"));
    }
    Err("cannot find a folder for settings; pass --config-dir".into())
}

/// Where files sent to this computer are saved unless told otherwise.
pub fn default_received_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(home) => PathBuf::from(home).join("Downloads").join("Screen Portals"),
        None => std::env::temp_dir().join("Screen Portals"),
    }
}

/// This computer's name as shown to others.
pub fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "computer".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("screen-portals-config-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Store::open(Some(dir)).unwrap()
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(to_hex(&[0, 255, 16]), "00ff10");
        assert_eq!(from_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(from_hex("0"), None);
        assert_eq!(from_hex("zz"), None);
    }

    #[test]
    fn settings_from_version_0_1_still_load() {
        let old = r#"{"peers":[{"key":"0707070707070707070707070707070707070707070707070707070707070707","name":"laptop","side":"right"}]}"#;
        let config: Config = serde_json::from_str(old).unwrap();
        assert_eq!(config.peers[0].placement(), Placement { side: Side::Right, offset: 0, mode: PortalMode::Edge });
        assert_eq!(config.peers[0].screen, None);
    }

    #[test]
    fn identity_is_created_once_and_reused() {
        let store = temp_store("identity");
        let first = store.identity().unwrap();
        let second = store.identity().unwrap();
        assert_eq!(first.public, second.public);
        assert_eq!(first.private, second.private);
        let _ = fs::remove_dir_all(store.dir());
    }

    #[test]
    fn config_round_trip_and_upsert() {
        let store = temp_store("config");
        assert_eq!(store.load_config().unwrap(), Config::default());
        let key = [7u8; 32];
        let mut config = Config::default();
        config.upsert(PeerInfo::new(&key, "old", Side::Left));
        let mut desk = PeerInfo::new(&key, "desk", Side::Right);
        desk.offset = -120;
        desk.mode = PortalMode::Start;
        desk.screen = Some([2560, 1440]);
        config.upsert(desk.clone());
        store.save_config(&config).unwrap();
        let loaded = store.load_config().unwrap();
        assert_eq!(loaded.peers.len(), 1);
        assert_eq!(loaded.peers[0], desk);
        assert_eq!(loaded.peers[0].placement(), Placement { side: Side::Right, offset: -120, mode: PortalMode::Start });
        assert_eq!(loaded.find(&key).unwrap().name, "desk");
        assert_eq!(loaded.find(&key).unwrap().key_bytes(), Some(key));
        assert!(loaded.find(&[8u8; 32]).is_none());
        let _ = fs::remove_dir_all(store.dir());
    }
}
