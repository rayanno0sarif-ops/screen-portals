//! `file://` addresses, the form in which Linux desktops put copied files on
//! the clipboard. Kept free of anything Linux-specific so it is tested on
//! every platform: paths are plain bytes here.

/// A path (as bytes, starting with `/`) written as a `file://` address.
/// Everything outside a small safe set is written as `%XX`.
pub fn file_uri(path: &[u8]) -> String {
    let mut uri = String::from("file://");
    for &byte in path {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// Undo `%XX` escapes. Broken escapes are kept as they are.
fn unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = (bytes[at] == b'%').then(|| text.get(at + 1..at + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    out
}

/// The local paths (as bytes) named by a list of addresses, one per line.
/// Lines that are comments, or that point at other computers, are skipped.
/// The first line of the list GNOME uses ("copy" or "cut") is skipped too.
pub fn paths_from_list(list: &str) -> Vec<Vec<u8>> {
    list.lines()
        .map(|line| line.trim_end_matches('\r').trim())
        .filter_map(|line| {
            let rest = line.strip_prefix("file://")?;
            // "file:///path" or "file://localhost/path"; never another host.
            let path = rest.strip_prefix("localhost").filter(|p| p.starts_with('/')).unwrap_or(rest);
            path.starts_with('/').then(|| unescape(path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_survive_a_round_trip() {
        let paths: Vec<&[u8]> = vec![b"/home/someone/one file.txt", "/tmp/caf\u{e9} \u{65e5}\u{672c}/100%,done#1?.bin".as_bytes(), b"/a/\xff\xfe"];
        let list: String = paths.iter().map(|p| file_uri(p) + "\r\n").collect();
        assert!(list.is_ascii());
        assert!(!list.contains(',') && !list.contains(' '));
        assert_eq!(paths_from_list(&list), paths);
    }

    #[test]
    fn well_known_forms_are_read() {
        assert_eq!(file_uri(b"/a b/c"), "file:///a%20b/c");
        let list = "# a comment\r\ncopy\nfile:///home/x/a%20b.txt\nfile://localhost/etc/hosts\nfile://elsewhere/share/x\nhttps://example.com/\nfile:///bad%zzescape\n";
        let want: Vec<&[u8]> = vec![b"/home/x/a b.txt", b"/etc/hosts", b"/bad%zzescape"];
        assert_eq!(paths_from_list(list), want);
    }
}
