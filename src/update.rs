//! Keeping the program up to date.
//!
//! Every change pushed to the project's main branch is built by GitHub and
//! published as a release. A build made that way knows its own number and
//! which repository it came from, and from time to time asks that
//! repository whether a higher number exists. If so it downloads the new
//! program, checks it arrived intact, swaps it in and restarts.
//!
//! **Who is trusted.** Updates are fetched over HTTPS from github.com only,
//! from the one repository this build was made from. That means whoever can
//! publish releases in that repository can change what runs on the
//! computers using it. The checksum guards against a damaged or cut-off
//! download; it is not a second lock.
//!
//! Builds made by hand (`cargo build`) carry no number and never update.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Set by the release workflow; absent in builds made by hand.
const BUILD: Option<&str> = option_env!("SCREEN_PORTALS_BUILD");
/// "owner/name" of the repository that built this program.
const REPO: Option<&str> = option_env!("SCREEN_PORTALS_REPO");

/// The file an installed copy replaces itself with.
pub const PROGRAM: &str = "ScreenPortals.exe";
/// The small file describing the newest release.
pub const DESCRIPTION: &str = "latest.json";
const MAX_PROGRAM: u64 = 64 * 1024 * 1024;
const MAX_DESCRIPTION: u64 = 4096;

/// This program's build number, if it is a published build.
pub fn this_build() -> Option<u64> {
    BUILD?.parse().ok()
}

fn tidy_repo(repo: &str) -> Option<&str> {
    let (owner, name) = repo.split_once('/')?;
    let plain = |part: &str| !part.is_empty() && part.len() <= 100 && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (plain(owner) && plain(name) && !name.contains("..")).then_some(repo)
}

/// Where this build looks for newer ones, if anywhere.
pub fn release_url() -> Option<String> {
    this_build()?;
    Some(format!("https://github.com/{}/releases/latest/download", tidy_repo(REPO?)?))
}

/// For showing to people: "build 12", or "local build".
pub fn version_text() -> String {
    match this_build() {
        Some(build) => format!("build {build}"),
        None => "local build".to_string(),
    }
}

/// What a release says about itself.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Latest {
    pub build: u64,
    /// SHA-256 of the program file, in hex.
    pub sha256: String,
    pub size: u64,
}

/// Read a release description, refusing anything that does not look like one.
pub fn parse_latest(bytes: &[u8]) -> Result<Latest, String> {
    // Windows tools like to put a marker at the start of text files.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let latest: Latest = serde_json::from_slice(bytes).map_err(|_| "the release description is not readable".to_string())?;
    let hex = latest.sha256.len() == 64 && latest.sha256.chars().all(|c| c.is_ascii_hexdigit());
    if !hex || latest.size == 0 || latest.size > MAX_PROGRAM {
        return Err("the release description does not make sense".into());
    }
    Ok(latest)
}

/// The SHA-256 checksum of some bytes, as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Ask `base` whether a build newer than `mine` exists and, if so, return
/// its number and the complete, checked program. `fetch` downloads one
/// address, refusing anything larger than the given size.
pub fn fetch_newer(mine: u64, base: &str, fetch: &dyn Fn(&str, u64) -> Result<Vec<u8>, String>) -> Result<Option<(u64, Vec<u8>)>, String> {
    let latest = parse_latest(&fetch(&format!("{base}/{DESCRIPTION}"), MAX_DESCRIPTION)?)?;
    if latest.build <= mine {
        return Ok(None);
    }
    let program = fetch(&format!("{base}/{PROGRAM}"), latest.size)?;
    if program.len() as u64 != latest.size || !sha256_hex(&program).eq_ignore_ascii_case(&latest.sha256) {
        return Err("the downloaded update did not arrive intact".into());
    }
    // An executable starts with "MZ"; anything else is not a program.
    if !program.starts_with(b"MZ") {
        return Err("the downloaded update is not a program".into());
    }
    Ok(Some((latest.build, program)))
}

fn beside(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Put the new program where the running one is. Windows will not let a
/// running program be overwritten, but it can be renamed, so the old one is
/// moved aside (and removed by `clean_up` on the next start).
pub fn swap_in(exe: &Path, program: &[u8]) -> Result<(), String> {
    let (fresh, old) = (beside(exe, ".new"), beside(exe, ".old"));
    std::fs::write(&fresh, program).map_err(|e| format!("cannot save the update: {e}"))?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&fresh);
        return Err(format!("cannot move the current program aside: {e}"));
    }
    if let Err(e) = std::fs::rename(&fresh, exe) {
        // Put things back exactly as they were.
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&fresh);
        return Err(format!("cannot put the update in place: {e}"));
    }
    Ok(())
}

/// Remove what an earlier update left behind.
pub fn clean_up(exe: &Path) {
    let _ = std::fs::remove_file(beside(exe, ".old"));
    let _ = std::fs::remove_file(beside(exe, ".new"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn description(build: u64, program: &[u8]) -> Vec<u8> {
        format!(r#"{{"build":{build},"sha256":"{}","size":{}}}"#, sha256_hex(program), program.len()).into_bytes()
    }

    #[test]
    fn checksum_matches_the_standard() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn only_tidy_repository_names_are_used() {
        assert_eq!(tidy_repo("someone/screen-portals"), Some("someone/screen-portals"));
        for bad in ["", "noslash", "/name", "owner/", "a/b/c", "owner/na me", "owner/..", "own?er/x", "owner/x#y", "owner/x@evil.example"] {
            assert_eq!(tidy_repo(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_build_made_by_hand_never_updates() {
        // The test build carries no number, exactly like `cargo build`.
        assert_eq!(this_build(), None);
        assert_eq!(release_url(), None);
        assert_eq!(version_text(), "local build");
    }

    #[test]
    fn descriptions_are_checked() {
        let good = description(7, b"MZ program");
        assert_eq!(parse_latest(&good).unwrap().build, 7);
        let with_marker = [&[0xEF, 0xBB, 0xBF][..], &good].concat();
        assert_eq!(parse_latest(&with_marker).unwrap().build, 7);
        assert!(parse_latest(b"<html>Not Found</html>").is_err());
        assert!(parse_latest(br#"{"build":7,"sha256":"abc","size":10}"#).is_err());
        assert!(parse_latest(format!(r#"{{"build":7,"sha256":"{}","size":0}}"#, "a".repeat(64)).as_bytes()).is_err());
        assert!(parse_latest(format!(r#"{{"build":7,"sha256":"{}","size":99999999999}}"#, "a".repeat(64)).as_bytes()).is_err());
    }

    #[test]
    fn a_newer_build_is_downloaded_and_checked() {
        let program = b"MZ this is the new program".to_vec();
        let asked = RefCell::new(Vec::new());
        let asked = &asked;
        let serve = |latest: Vec<u8>, body: Vec<u8>| {
            move |url: &str, limit: u64| -> Result<Vec<u8>, String> {
                asked.borrow_mut().push((url.to_string(), limit));
                if url.ends_with("latest.json") { Ok(latest.clone()) } else { Ok(body.clone()) }
            }
        };

        // Same or older: nothing more is downloaded.
        let fetch = serve(description(5, &program), program.clone());
        assert_eq!(fetch_newer(5, "https://x", &fetch).unwrap(), None);
        assert_eq!(fetch_newer(9, "https://x", &fetch).unwrap(), None);
        assert_eq!(asked.borrow().len(), 2, "only the description was fetched");

        // Newer: the program comes back, and was asked for with its exact size as the limit.
        assert_eq!(fetch_newer(4, "https://x", &fetch).unwrap(), Some((5, program.clone())));
        assert_eq!(asked.borrow().last().unwrap(), &("https://x/ScreenPortals.exe".to_string(), program.len() as u64));

        // Damaged, cut short, swapped or not a program at all: refused.
        let damaged = serve(description(5, &program), b"MZ this is the new progrAm".to_vec());
        assert!(fetch_newer(4, "https://x", &damaged).is_err());
        let short = serve(description(5, &program), program[..10].to_vec());
        assert!(fetch_newer(4, "https://x", &short).is_err());
        let page = b"<html>error page</html>".to_vec();
        let not_program = serve(description(5, &page), page.clone());
        assert!(fetch_newer(4, "https://x", &not_program).is_err());
        let failing = |_: &str, _: u64| -> Result<Vec<u8>, String> { Err("offline".into()) };
        assert_eq!(fetch_newer(4, "https://x", &failing), Err("offline".to_string()));
    }

    #[test]
    fn swapping_replaces_the_program_and_can_be_cleaned_up() {
        let dir = std::env::temp_dir().join(format!("screen-portals-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("ScreenPortals.exe");
        std::fs::write(&exe, "old").unwrap();
        // Leftovers from an earlier update do not get in the way.
        std::fs::write(dir.join("ScreenPortals.exe.old"), "older").unwrap();

        swap_in(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(dir.join("ScreenPortals.exe.old")).unwrap(), b"old");
        assert!(!dir.join("ScreenPortals.exe.new").exists());

        clean_up(&exe);
        assert!(!dir.join("ScreenPortals.exe.old").exists());
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");

        // Nowhere to write: reported, and nothing is left half done.
        assert!(swap_in(&dir.join("missing").join("x.exe"), b"new").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
