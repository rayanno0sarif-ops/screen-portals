//! The settings window's back end: a tiny web server that only this computer
//! can reach, serving one page and a handful of JSON calls.
//!
//! It listens on 127.0.0.1 only. Every call must carry a random token that
//! is handed to the window when it is opened, and must name this server in
//! its Host header, so a web page open in a browser cannot poke at it.

use crate::config::{from_hex, to_hex, Key};
use crate::layout::{Placement, PortalMode, Side};
use crate::node::{Confirm, Node};
use crate::random_bytes;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const PAGE: &str = include_str!("../ui/index.html");
const MAX_REQUEST: usize = 64 * 1024;
/// The one call whose body is a whole file rather than a little JSON.
const SEND_PATH: &str = "/api/send";
/// A little shorter than the engine's own pairing timeout.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(110);

/// The pairing question, parked here until the window answers it.
#[derive(Default)]
pub struct PairPrompt {
    state: Mutex<PromptState>,
    changed: Condvar,
}

#[derive(Default)]
struct PromptState {
    pending: Option<(String, String)>,
    answer: Option<bool>,
}

impl PairPrompt {
    /// A prompt with nothing being asked.
    pub fn new() -> Arc<PairPrompt> {
        Arc::new(PairPrompt::default())
    }

    /// The callback to give the engine. It blocks until the user answers in
    /// the window, or gives up (declining) after a couple of minutes.
    pub fn confirm_fn(self: &Arc<Self>) -> Confirm {
        let prompt = self.clone();
        Box::new(move |name: &str, code: &str| prompt.ask(name, code))
    }

    fn ask(&self, name: &str, code: &str) -> bool {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        let mut state = self.state.lock().unwrap();
        state.pending = Some((name.to_string(), code.to_string()));
        state.answer = None;
        while state.answer.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
        state.pending = None;
        state.answer.take().unwrap_or(false)
    }

    /// The question currently waiting, as (computer name, code).
    pub fn current(&self) -> Option<(String, String)> {
        self.state.lock().unwrap().pending.clone()
    }

    /// The user's answer from the window. Ignored if nothing is being asked.
    pub fn answer(&self, accept: bool) {
        let mut state = self.state.lock().unwrap();
        if state.pending.is_some() {
            state.answer = Some(accept);
            self.changed.notify_all();
        }
    }
}

/// Where the settings window can be reached.
pub struct UiServer {
    pub port: u16,
    pub token: String,
}

impl UiServer {
    /// Address to open in a window. The token rides in the fragment, which
    /// browsers never send over the network or to other sites.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/#{}", self.port, self.token)
    }
}

struct Shared {
    node: Arc<Node>,
    prompt: Arc<PairPrompt>,
    token: String,
    port: u16,
    quit: Box<dyn Fn() + Send + Sync>,
}

/// Start serving. `quit` is called when the user chooses to exit from the window.
pub fn serve(node: Arc<Node>, prompt: Arc<PairPrompt>, quit: Box<dyn Fn() + Send + Sync>) -> Result<UiServer, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("cannot open the settings window: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = to_hex(&random_bytes::<16>());
    let shared = Arc::new(Shared { node, prompt, token: token.clone(), port, quit });
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = shared.clone();
            thread::spawn(move || {
                let _ = handle(stream, &shared);
            });
        }
    });
    Ok(UiServer { port, token })
}

struct Request {
    method: String,
    path: String,
    host: String,
    token: String,
    /// For a file being sent: which computer it goes to and what it is called.
    peer: String,
    file_name: String,
    length: u64,
    /// The body, or for a file only the part that arrived with the headers.
    body: Vec<u8>,
}

/// Undo percent-encoding (the window encodes file names so that any name
/// fits in a header).
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if data.len() > MAX_REQUEST {
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        data.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&data[..head_end]).ok()?.to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let (mut host, mut token, mut peer, mut file_name, mut length) = (String::new(), String::new(), String::new(), String::new(), 0u64);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => host = value.to_string(),
            "x-portal-token" => token = value.to_string(),
            "x-portal-peer" => peer = value.to_string(),
            "x-portal-file" => file_name = percent_decode(value)?,
            "content-length" => length = value.parse().ok()?,
            _ => {}
        }
    }
    let mut body = data[head_end + 4..].to_vec();
    // A file is passed on piece by piece later instead of being held here.
    if !(method == "POST" && path == SEND_PATH) {
        if length > MAX_REQUEST as u64 {
            return None;
        }
        while (body.len() as u64) < length {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    body.truncate(length.min(body.len() as u64) as usize);
    Some(Request { method, path, host, token, peer, file_name, length, body })
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, extra: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)
}

fn respond_json(stream: &mut TcpStream, status: &str, value: Value) -> std::io::Result<()> {
    respond(stream, status, "application/json", "", value.to_string().as_bytes())
}

/// Compare without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Deserialize)]
struct LayoutBody {
    key: String,
    side: Side,
    offset: i32,
    mode: PortalMode,
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

#[derive(Deserialize)]
struct FlagBody {
    on: bool,
}

#[derive(Deserialize)]
struct SpeakersBody {
    /// The chosen computer, or nothing for "each computer plays its own".
    key: Option<String>,
}

#[derive(Deserialize)]
struct IdBody {
    id: u32,
}

fn parse_key(text: &str) -> Result<Key, String> {
    from_hex(text).and_then(|v| v.try_into().ok()).ok_or_else(|| "malformed computer id".to_string())
}

fn body<T: for<'a> Deserialize<'a>>(request: &Request) -> Result<T, String> {
    serde_json::from_slice(&request.body).map_err(|_| "malformed request".to_string())
}

fn state(shared: &Shared) -> Value {
    let mut value = serde_json::to_value(shared.node.status()).unwrap_or_else(|_| json!({}));
    value["prompt"] = match shared.prompt.current() {
        Some((name, code)) => json!({ "name": name, "code": code }),
        None => Value::Null,
    };
    value["can_pair"] = json!(shared.node.can_pair());
    value["version"] = json!(crate::update::version_text());
    value
}

fn api(shared: &Shared, request: &Request) -> Result<Value, String> {
    let node = &shared.node;
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/api/state") => {}
        ("POST", "/api/layout") => {
            let b: LayoutBody = body(request)?;
            node.set_placement(&parse_key(&b.key)?, Placement { side: b.side, offset: b.offset, mode: b.mode })?;
        }
        ("POST", "/api/pairing") => {
            let b: FlagBody = body(request)?;
            if !node.set_pairing(b.on) {
                return Err("pairing is not available".into());
            }
            if !b.on {
                shared.prompt.answer(false);
            }
        }
        ("POST", "/api/pairing/answer") => {
            let b: FlagBody = body(request)?;
            shared.prompt.answer(b.on);
        }
        ("POST", "/api/forget") => {
            let b: KeyBody = body(request)?;
            node.forget_key(&parse_key(&b.key)?)?;
        }
        ("POST", "/api/pause") => {
            let b: FlagBody = body(request)?;
            node.set_paused(b.on);
        }
        ("POST", "/api/speakers") => {
            let b: SpeakersBody = body(request)?;
            let key = match b.key {
                Some(text) => Some(parse_key(&text)?),
                None => None,
            };
            node.set_speakers(key)?;
        }
        ("POST", "/api/transfer/cancel") => {
            let b: IdBody = body(request)?;
            node.cancel_transfer(b.id);
        }
        ("POST", "/api/transfer/reveal") => {
            // Only the id travels: the engine knows which file it saved.
            let b: IdBody = body(request)?;
            if !node.reveal_transfer(b.id) {
                return Err("that file is no longer known".into());
            }
        }
        ("POST", "/api/quit") => {
            node.shutdown();
            (shared.quit)();
        }
        _ => return Err("no such call".into()),
    }
    Ok(state(shared))
}

/// A file handed over by the window: pass it on to the chosen computer as
/// it arrives, without ever holding the whole of it.
fn send_file(shared: &Shared, request: &Request, stream: &mut TcpStream) -> Result<Value, String> {
    let key = parse_key(&request.peer)?;
    if request.file_name.is_empty() {
        return Err("the file has no name".into());
    }
    let already = request.body.as_slice();
    let mut source = already.chain(stream.take(request.length - already.len() as u64));
    shared.node.send_stream(&key, &request.file_name, request.length, &mut source)?;
    Ok(state(shared))
}

fn handle(mut stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let Some(request) = read_request(&mut stream) else {
        return respond(&mut stream, "400 Bad Request", "text/plain", "", b"bad request");
    };
    // Refuse anything that reached us under another name (DNS rebinding).
    let expected = [format!("127.0.0.1:{}", shared.port), format!("localhost:{}", shared.port)];
    if !expected.contains(&request.host) {
        return respond(&mut stream, "403 Forbidden", "text/plain", "", b"forbidden");
    }
    if request.path.starts_with("/api/") {
        if !same(&request.token, &shared.token) {
            return respond_json(&mut stream, "403 Forbidden", json!({ "error": "forbidden" }));
        }
        let outcome = if request.method == "POST" && request.path == SEND_PATH {
            send_file(shared, &request, &mut stream)
        } else {
            api(shared, &request)
        };
        return match outcome {
            Ok(value) => respond_json(&mut stream, "200 OK", value),
            Err(message) => respond_json(&mut stream, "400 Bad Request", json!({ "error": message })),
        };
    }
    if request.method == "GET" && (request.path == "/" || request.path == "/index.html") {
        let policy = "Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                      connect-src 'self'; img-src data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\
                      Referrer-Policy: no-referrer\r\n";
        return respond(&mut stream, "200 OK", "text/html; charset=utf-8", policy, PAGE.as_bytes());
    }
    respond(&mut stream, "404 Not Found", "text/plain", "", b"not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_waits_for_an_answer() {
        let prompt = PairPrompt::new();
        assert_eq!(prompt.current(), None);
        prompt.answer(true); // nothing is being asked, so this is ignored
        let confirm = prompt.confirm_fn();
        let asker = thread::spawn(move || confirm("laptop", "123456"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while prompt.current().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(prompt.current(), Some(("laptop".into(), "123456".into())));
        prompt.answer(true);
        assert!(asker.join().unwrap());
        assert_eq!(prompt.current(), None);
    }

    #[test]
    fn token_comparison() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert!(!same("", "a"));
    }
}
