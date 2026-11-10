//! Files travelling between computers: what may be sent, and the rules the
//! receiving side applies before anything touches its disk.
//!
//! The other computer is paired, but it is still treated as able to send
//! nonsense. Paths are rebuilt from checked parts, sizes are enforced, and
//! everything lands in a fresh private folder until it has fully arrived.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Size of one piece of file data on the wire.
pub const CHUNK: usize = 32 * 1024;
/// How far the sender may run ahead of what the receiver has confirmed.
/// A key press is never queued behind more than this.
pub const WINDOW: u64 = 8 * CHUNK as u64;
/// Most files and folders one transfer may contain.
pub const MAX_ENTRIES: u32 = 20_000;
/// Largest picture, as PNG bytes.
pub const MAX_IMAGE: u64 = 64 * 1024 * 1024;
/// Copied files up to this total size follow the cursor on their own.
pub const DEFAULT_CARRY_LIMIT: u64 = 1024 * 1024 * 1024;

const MAX_DEPTH: usize = 32;
const MAX_PART: usize = 200;
const MAX_PATH_CHARS: usize = 1024;

fn safe_part(part: &str) -> bool {
    if part.is_empty() || part == "." || part == ".." || part.chars().count() > MAX_PART {
        return false;
    }
    if part.chars().any(|c| c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return false;
    }
    // Windows quietly drops trailing dots and spaces, which would let two
    // different names mean the same file.
    if part.ends_with('.') || part.ends_with(' ') {
        return false;
    }
    // Device names are special in every folder, with or without an extension.
    let stem = part.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix).map_or(false, |rest| rest.len() == 1 && rest.chars().all(|c| c.is_ascii_digit() && c != '0'))
    };
    !(matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered("COM") || numbered("LPT"))
}

/// Split a path from the network into parts that are safe to create inside
/// a folder of ours, or refuse it.
pub fn safe_components(path: &str) -> Option<Vec<String>> {
    if path.chars().count() > MAX_PATH_CHARS {
        return None;
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > MAX_DEPTH || !parts.iter().all(|p| safe_part(p)) {
        return None;
    }
    Some(parts.into_iter().map(str::to_string).collect())
}

/// A name for a single received file: the safe one it came with, or a
/// harmless stand-in.
pub fn safe_name(name: &str) -> String {
    if safe_part(name) {
        name.to_string()
    } else {
        "received file".to_string()
    }
}

/// A web address that is safe to hand to a browser: plain http or https,
/// of sensible length, with nothing that could be read as anything else.
pub fn safe_url(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"))?;
    let odd = |c: char| c.is_control() || c.is_whitespace() || matches!(c, '"' | '<' | '>' | '\\' | '^' | '`' | '|');
    (text.len() <= 2048 && !rest.is_empty() && !text.chars().any(odd)).then(|| text.to_string())
}

fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

/// A path in `dir` for `name` that does not exist yet: "a.txt", "a (2).txt"...
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    (2..10_000).map(|n| dir.join(numbered_name(name, n))).find(|p| !p.exists()).unwrap_or(first)
}

// ---------- sending side ----------

/// One file or folder about to be sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// Path as the receiver will see it, with `/` between parts.
    pub rel: String,
    pub source: PathBuf,
    pub size: u64,
    pub dir: bool,
}

/// Everything one transfer will send, in sending order.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub items: Vec<Item>,
    pub files: u32,
    pub bytes: u64,
    /// Name of the first thing, for showing to people.
    pub label: String,
}

/// Why a selection of files is not being sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    Empty,
    /// More than the automatic limit; the limit is included.
    TooLarge(u64),
    TooMany,
    Unreadable(String),
}

/// List everything under the given files and folders. Stops as soon as the
/// total passes `limit`, so a huge folder is not walked to the end.
pub fn manifest(paths: &[PathBuf], limit: u64) -> Result<Manifest, Refusal> {
    let mut items = Vec::new();
    let (mut files, mut bytes) = (0u32, 0u64);
    let mut taken: HashSet<String> = HashSet::new();
    let mut label = String::new();
    // (source, path for the receiver)
    let mut todo: Vec<(PathBuf, String)> = Vec::new();

    for path in paths.iter().rev() {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Err(Refusal::Unreadable(path.display().to_string()));
        };
        todo.push((path.clone(), name.to_string()));
    }
    // Two things with the same name from different folders get distinct names.
    for (_, name) in todo.iter_mut().rev() {
        let mut candidate = name.clone();
        let mut n = 2;
        while !taken.insert(candidate.to_lowercase()) {
            candidate = numbered_name(name, n);
            n += 1;
        }
        *name = candidate;
    }

    while let Some((source, rel)) = todo.pop() {
        let unreadable = || Refusal::Unreadable(source.display().to_string());
        let meta = fs::symlink_metadata(&source).map_err(|_| unreadable())?;
        // Links are not followed: they could lead anywhere, or in circles.
        if meta.file_type().is_symlink() {
            continue;
        }
        if safe_components(&rel).is_none() {
            return Err(unreadable());
        }
        if label.is_empty() {
            label = rel.clone();
        }
        if items.len() as u32 >= MAX_ENTRIES {
            return Err(Refusal::TooMany);
        }
        if meta.is_dir() {
            items.push(Item { rel: rel.clone(), source: source.clone(), size: 0, dir: true });
            let mut children: Vec<PathBuf> =
                fs::read_dir(&source).map_err(|_| unreadable())?.filter_map(|e| e.ok().map(|e| e.path())).collect();
            children.sort();
            for child in children.into_iter().rev() {
                let Some(name) = child.file_name().and_then(|n| n.to_str()) else { return Err(unreadable()) };
                let child_rel = format!("{rel}/{name}");
                todo.push((child, child_rel));
            }
        } else if meta.is_file() {
            bytes = bytes.saturating_add(meta.len());
            if bytes > limit {
                return Err(Refusal::TooLarge(limit));
            }
            files += 1;
            items.push(Item { rel, source, size: meta.len(), dir: false });
        }
    }
    if items.is_empty() {
        return Err(Refusal::Empty);
    }
    Ok(Manifest { items, files, bytes, label })
}

// ---------- receiving side ----------

/// Collects one incoming transfer into a fresh private folder. If it is
/// dropped before `finish`, everything it wrote is removed again.
pub struct Receiver {
    root: PathBuf,
    entries_left: u32,
    /// Bytes promised in total that no entry has claimed yet.
    unclaimed: u64,
    /// The file being written and how much of it is still to come.
    current: Option<(File, u64)>,
    tops: Vec<PathBuf>,
    received: u64,
    total: u64,
    keep: bool,
}

impl Receiver {
    /// `root` must not exist yet; its parent is created if needed.
    pub fn new(root: PathBuf, entries: u32, bytes: u64) -> Result<Receiver, String> {
        if entries == 0 || entries > MAX_ENTRIES {
            return Err("unreasonable number of files".into());
        }
        if let Some(parent) = root.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        fs::create_dir(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
        Ok(Receiver { root, entries_left: entries, unclaimed: bytes, current: None, tops: Vec::new(), received: 0, total: bytes, keep: false })
    }

    /// Bytes written to disk so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Start the next file or folder.
    pub fn entry(&mut self, path: &str, size: u64, dir: bool) -> Result<(), String> {
        if self.current.is_some() {
            return Err("a file was cut short".into());
        }
        if self.entries_left == 0 {
            return Err("more files than announced".into());
        }
        self.entries_left -= 1;
        let parts = safe_components(path).ok_or("a file has a name this computer cannot accept")?;
        let mut target = self.root.clone();
        target.extend(&parts);
        let top = self.root.join(&parts[0]);
        if dir {
            if size != 0 {
                return Err("a folder cannot have a size".into());
            }
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
        } else {
            if size > self.unclaimed {
                return Err("more data than announced".into());
            }
            self.unclaimed -= size;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            // Never opens something that already exists.
            let file = OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| e.to_string())?;
            if size > 0 {
                self.current = Some((file, size));
            }
        }
        if !self.tops.contains(&top) {
            self.tops.push(top);
        }
        Ok(())
    }

    /// More of the current file. Returns the running total received.
    pub fn data(&mut self, data: &[u8]) -> Result<u64, String> {
        let Some((file, left)) = self.current.as_mut() else {
            return Err("data without a file".into());
        };
        if data.len() as u64 > *left {
            return Err("more data than announced".into());
        }
        file.write_all(data).map_err(|e| format!("cannot write: {e}"))?;
        *left -= data.len() as u64;
        self.received += data.len() as u64;
        if *left == 0 {
            self.current = None;
        }
        Ok(self.received)
    }

    /// Everything has arrived. Returns the top-level files and folders,
    /// which stay on disk from here on.
    pub fn finish(mut self) -> Result<Vec<PathBuf>, String> {
        if self.current.is_some() || self.unclaimed != 0 || self.received != self.total || self.tops.is_empty() {
            return Err("the transfer ended early".into());
        }
        self.keep = true;
        Ok(std::mem::take(&mut self.tops))
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if !self.keep {
            self.current = None; // close the file before removing it
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("screen-portals-transfer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_plain_relative_paths_are_accepted() {
        assert_eq!(safe_components("notes.txt"), Some(vec!["notes.txt".to_string()]));
        assert_eq!(safe_components("photos/2024/beach day.jpg").unwrap().len(), 3);
        assert!(safe_components(".hidden").is_some());
        for bad in [
            "",
            "..",
            "../escape.txt",
            "a/../../b",
            "a//b",
            "/absolute",
            "C:/Windows/system32",
            "C:evil",
            "a\\b",
            "stream.txt:secret",
            "trailing.",
            "trailing ",
            "nul",
            "NUL.txt",
            "folder/com1",
            "LPT9.log",
            "what?.txt",
            "tab\there",
            "line\nbreak",
        ] {
            assert_eq!(safe_components(bad), None, "{bad:?} must be refused");
        }
        // Names that merely resemble device names are fine.
        assert!(safe_components("console.log").is_some());
        assert!(safe_components("COM10").is_some());
        assert!(safe_components(&"x/".repeat(40)).is_none());
        assert_eq!(safe_name("ok.zip"), "ok.zip");
        assert_eq!(safe_name("..\\..\\evil.exe"), "received file");
    }

    #[test]
    fn only_web_addresses_are_opened() {
        assert_eq!(safe_url("https://example.com/a?b=c#d").as_deref(), Some("https://example.com/a?b=c#d"));
        assert!(safe_url("HTTP://Example.com").is_some());
        for bad in ["", "https://", "file:///C:/Windows/system32/calc.exe", "javascript:alert(1)", "example.com", "https://a b", "https://a\"b", "ms-settings:", "C:\\x.exe", "https://x\ny", " https://x"] {
            assert_eq!(safe_url(bad), None, "{bad:?}");
        }
        assert!(safe_url(&format!("https://x/{}", "a".repeat(3000))).is_none());
    }

    #[test]
    fn unique_names_never_overwrite() {
        let dir = scratch("unique");
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        fs::write(dir.join("a.txt"), "1").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        fs::write(dir.join("a (2).txt"), "2").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (3).txt"));
        fs::create_dir(dir.join("folder")).unwrap();
        assert_eq!(unique_path(&dir, "folder"), dir.join("folder (2)"));
        assert_eq!(unique_path(&dir, ".profile"), dir.join(".profile"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn manifest_lists_folders_before_their_contents() {
        let dir = scratch("manifest");
        fs::create_dir_all(dir.join("album/inner")).unwrap();
        fs::write(dir.join("album/a.txt"), "aaaa").unwrap();
        fs::write(dir.join("album/inner/b.txt"), "bb").unwrap();
        fs::write(dir.join("album/empty.txt"), "").unwrap();
        fs::write(dir.join("single.bin"), [7u8; 10]).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::write(dir.join("other/single.bin"), [8u8; 3]).unwrap();

        let picked = [dir.join("album"), dir.join("single.bin"), dir.join("other/single.bin")];
        let m = manifest(&picked, 1000).unwrap();
        let rels: Vec<&str> = m.items.iter().map(|i| i.rel.as_str()).collect();
        assert_eq!(
            rels,
            ["album", "album/a.txt", "album/empty.txt", "album/inner", "album/inner/b.txt", "single.bin", "single (2).bin"]
        );
        assert_eq!((m.files, m.bytes), (5, 4 + 2 + 10 + 3));
        assert_eq!(m.label, "album");
        assert!(m.items[0].dir && !m.items[1].dir);

        assert_eq!(manifest(&picked, 10), Err(Refusal::TooLarge(10)));
        assert_eq!(manifest(&[], 10), Err(Refusal::Empty));
        assert!(matches!(manifest(&[dir.join("missing")], 10), Err(Refusal::Unreadable(_))));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn receiver_writes_exactly_what_was_announced() {
        let dir = scratch("receive");
        let root = dir.join("in");
        let mut r = Receiver::new(root.clone(), 4, 7).unwrap();
        r.entry("docs", 0, true).unwrap();
        r.entry("docs/a.txt", 5, false).unwrap();
        assert_eq!(r.data(b"hel").unwrap(), 3);
        assert_eq!(r.data(b"lo").unwrap(), 5);
        r.entry("docs/empty", 0, false).unwrap();
        r.entry("b.bin", 2, false).unwrap();
        r.data(&[1, 2]).unwrap();
        let tops = r.finish().unwrap();
        assert_eq!(tops, vec![root.join("docs"), root.join("b.bin")]);
        assert_eq!(fs::read_to_string(root.join("docs/a.txt")).unwrap(), "hello");
        assert_eq!(fs::read(root.join("docs/empty")).unwrap(), b"");
        assert_eq!(fs::read(root.join("b.bin")).unwrap(), [1, 2]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn receiver_refuses_tricks_and_cleans_up() {
        let dir = scratch("refuse");
        fs::write(dir.join("precious.txt"), "keep me").unwrap();

        let attempt = |tag: &str, run: &dyn Fn(&mut Receiver) -> Result<(), String>| {
            let root = dir.join(tag);
            let mut r = Receiver::new(root.clone(), 3, 10).unwrap();
            assert!(run(&mut r).is_err(), "{tag} must be refused");
            drop(r);
            assert!(!root.exists(), "{tag}: nothing may be left behind");
        };
        attempt("climb", &|r| r.entry("../precious.txt", 1, false));
        attempt("absolute", &|r| r.entry("C:/precious.txt", 1, false));
        attempt("too-big", &|r| r.entry("a", 11, false));
        attempt("overflow", &|r| r.entry("a", 2, false).and_then(|_| r.data(b"abc").map(|_| ())));
        attempt("no-file", &|r| r.data(b"abc").map(|_| ()));
        attempt("cut-short", &|r| r.entry("a", 5, false).and_then(|_| r.entry("b", 1, false)));
        attempt("same-twice", &|r| r.entry("a", 0, false).and_then(|_| r.entry("a", 0, false)));
        attempt("sized-folder", &|r| r.entry("a", 3, true));
        attempt("too-many", &|r| {
            for name in ["a", "b", "c"] {
                r.entry(name, 0, false)?;
            }
            r.entry("d", 0, false)
        });

        // Ending before everything arrived is refused too.
        let root = dir.join("early");
        let mut r = Receiver::new(root.clone(), 1, 10).unwrap();
        r.entry("a", 10, false).unwrap();
        r.data(b"12345").unwrap();
        assert!(r.finish().is_err());
        assert!(!root.exists());

        assert!(Receiver::new(dir.join("zero"), 0, 0).is_err());
        assert!(Receiver::new(dir.join("flood"), MAX_ENTRIES + 1, 0).is_err());
        // The private folder must be new: an existing one is never reused.
        assert!(Receiver::new(dir.clone(), 1, 0).is_err());
        assert_eq!(fs::read_to_string(dir.join("precious.txt")).unwrap(), "keep me");
        let _ = fs::remove_dir_all(dir);
    }
}
