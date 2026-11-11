//! The engine: connections to paired computers, the "is my cursor here or
//! over there" state, and routing of input in both directions.

use crate::audio::{self, SilenceGate};
use crate::config::{fingerprint, from_hex, to_hex, Config, Identity, Key, PeerInfo, Store};
use crate::datagram::{self, Opener, Sealer};
use crate::keymap;
use crate::layout::{self, Placement, Portal, PortalMode, Remote, Side};
use crate::pairing;
use crate::picture;
use crate::platform::{Caught, DragPayload, Platform};
use crate::proto::BUTTON_LEFT;
use crate::proto::{Dgram, Msg, TransferKind, PROTOCOL_VERSION};
use crate::random_bytes;
use crate::secure::{self, Established, SecureStream};
use crate::transfer::{self, Manifest, Receiver, Refusal};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// TCP port the encrypted connection listens on unless told otherwise.
pub const DEFAULT_TCP_PORT: u16 = 47811;

/// Transfers remembered for the settings window.
const MAX_RECORDS: usize = 12;
/// Transfers one computer may have under way towards us at once.
const MAX_INCOMING: usize = 4;
/// Give up on a transfer when the other side confirms nothing for this long.
const STALL_TIMEOUT: Duration = Duration::from_secs(20);
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);

/// The left button in the set of buttons held on this computer.
const LEFT_HELD: u8 = 1 << BUTTON_LEFT;
/// Dragged text larger than this is not carried.
const MAX_DRAG_TEXT: usize = 1024 * 1024;

/// Whether the mouse button that is carrying dragged files is still down
/// while those files are on their way.
#[derive(Clone, Copy, PartialEq)]
enum Hold {
    None,
    Holding,
    Released,
}

/// Why the platform was asked to look for a drag.
enum Purpose {
    /// Our own cursor is at a doorway with the button held: if something is
    /// being dragged it should cross with the cursor.
    Cross { link: Arc<Link>, portal: Portal, side: Side, t: f64 },
    /// The computer controlling us took its cursor home with the button
    /// held: whatever it was dragging here should follow it.
    Return { link: Arc<Link> },
}

#[derive(Default)]
struct DragWatch {
    next_token: u64,
    waiting: Option<(u64, Purpose)>,
    /// This press of the button has already been looked at once.
    tried: bool,
}

// Why a transfer was told to stop.
const RUNNING: u8 = 0;
const BY_USER: u8 = 1;
const BY_PEER: u8 = 2;
const LINK_LOST: u8 = 3;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PAIR_TIMEOUT: Duration = Duration::from_secs(120);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A link with no traffic for this long is considered dead. Pings go out
/// every `TICK`, so this only triggers when the other side has really gone.
const IDLE_TIMEOUT: Duration = Duration::from_secs(6);
const TICK: Duration = Duration::from_secs(2);
const MAX_CLIPBOARD: usize = 4 * 1024 * 1024;
const ALREADY_CONNECTED: &str = "already connected";

/// Asked during pairing: (other computer's name, six-digit code) -> accept?
/// It may block while the person decides.
pub type Confirm = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// How this computer goes about pairing with new ones.
pub struct PairingOptions {
    /// Which edge of this desktop leads to the computer being paired, if the
    /// user said so up front. Otherwise it can be arranged afterwards.
    pub side: Option<Side>,
    pub confirm: Confirm,
    /// Start in pairing mode. It can also be switched on later.
    pub active: bool,
}

/// Everything needed to start the engine.
pub struct Options {
    pub name: String,
    pub bind: IpAddr,
    /// Preferred TCP port; another free port is used if it is taken.
    pub tcp_port: u16,
    /// Announce on the LAN and connect to paired computers automatically.
    pub discovery: bool,
    /// How to ask the user about new computers. Without it this computer
    /// can never pair.
    pub pairing: Option<PairingOptions>,
    /// Extra "host:port" addresses to dial, for networks without discovery.
    pub connect: Vec<String>,
    /// Where files sent to this computer are saved. `None` means a
    /// "Screen Portals" folder inside the user's Downloads.
    pub received_dir: Option<PathBuf>,
}

/// What the local clipboard holds that can follow the cursor.
#[derive(Clone)]
enum Carry {
    Nothing,
    Image,
    Files(Vec<PathBuf>),
}

struct Clip {
    local: Carry,
    /// Computers that already have `local`.
    carried: HashSet<Key>,
    /// Clipboard events are numbered so that the newest copy always wins,
    /// whichever order slow transfers happen to finish in.
    stamp: u64,
    latest: u64,
}

impl Clip {
    /// Something was copied on this computer (or text arrived for it).
    fn changed(&mut self, local: Carry) {
        self.stamp += 1;
        self.latest = self.stamp;
        self.local = local;
        self.carried.clear();
    }

    /// A transfer towards this clipboard starts; this is its place in line.
    fn ticket(&mut self) -> u64 {
        self.stamp += 1;
        self.stamp
    }

    /// The transfer is complete: may it replace the clipboard?
    fn admit(&mut self, ticket: u64) -> bool {
        if ticket <= self.latest {
            return false;
        }
        self.latest = ticket;
        self.local = Carry::Nothing;
        self.carried.clear();
        true
    }
}

/// Where a transfer has got to.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TransferState {
    Active,
    Done,
    Failed,
    Cancelled,
    /// Never started, for the reason in `note`.
    Skipped,
}

/// One transfer as the settings window shows it.
#[derive(Serialize, Debug, Clone)]
pub struct TransferStatus {
    pub id: u32,
    pub peer: String,
    pub peer_key: String,
    pub incoming: bool,
    pub kind: TransferKind,
    /// Name of the first file or folder.
    pub label: String,
    pub files: u32,
    pub bytes: u64,
    pub done: u64,
    pub state: TransferState,
    pub note: Option<String>,
    /// A received file can be shown in its folder.
    pub can_reveal: bool,
}

struct Record {
    status: TransferStatus,
    reveal: Option<PathBuf>,
    stop: Arc<AtomicU8>,
}

/// A transfer arriving from another computer.
struct Incoming {
    record: u32,
    kind: TransferKind,
    stop: Arc<AtomicU8>,
    files: Option<Receiver>,
    image: Vec<u8>,
    bytes: u64,
    ticket: u64,
}

/// What the receiving side has told us about a transfer we are sending.
struct Outgoing {
    /// (bytes confirmed, everything in place)
    progress: Mutex<(u64, bool)>,
    moved: Condvar,
    stop: Arc<AtomicU8>,
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 || value.fract() < 0.05 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One live, authenticated connection to a paired computer.
pub struct Link {
    pub key: Key,
    pub name: String,
    placement: Mutex<Placement>,
    /// The doorway to this computer; `None` if the arrangement leaves none.
    portal: Mutex<Option<Portal>>,
    stream: SecureStream,
    screen_w: i32,
    screen_h: i32,
    sealer: Sealer,
    opener: Opener,
    peer_session: u64,
    udp_addr: SocketAddr,
    /// The other side has confirmed that our datagrams reach it.
    udp_ok: AtomicBool,
    /// We have told the other side that its datagrams reach us.
    probe_acked: AtomicBool,
    /// True when the computer with the lower key dialled this connection.
    /// Used to pick the same survivor on both ends if two connect at once.
    by_lower: bool,
    injected_keys: Mutex<HashSet<u16>>,
    injected_buttons: Mutex<HashSet<u8>>,
    incoming: Mutex<HashMap<u64, Incoming>>,
    outgoing: Mutex<HashMap<u64, Arc<Outgoing>>>,
    /// The other computer's cursor is on our screen right now.
    visiting: AtomicBool,
    drag_hold: Mutex<Hold>,
}

enum Out {
    Tcp(Arc<Link>, Msg),
    Mouse(Arc<Link>, u16, u16),
    Probe(Arc<Link>),
    Audio(Arc<Link>, Dgram),
}

/// Where this computer's own sound is going, if not to its own speakers.
struct AudioOut {
    to: Option<Arc<Link>>,
    seq: u32,
    gate: SilenceGate,
}

/// What is happening with this computer's own sound.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SoundState {
    /// This computer plays its own sound (and any sent to it).
    Own,
    /// This computer is silent; its sound plays on the chosen computer.
    Sending,
    /// Another computer was chosen but cannot be reached right now, so
    /// sound plays here meanwhile.
    Waiting,
}

/// Where this computer's own mouse and keyboard are currently going.
enum Ctl {
    Local {
        /// Mouse buttons held; we never switch computers mid-drag.
        buttons: u8,
        /// Keys physically held, so modifiers can follow the cursor across.
        held: HashSet<u16>,
        /// Consecutive pointer events resting on an outer edge.
        streak: u8,
    },
    Remote {
        link: Arc<Link>,
        pos: Remote,
        /// The doorway we came through, for mapping the way back.
        portal: Portal,
        /// Keys and buttons currently down on the other computer.
        keys: HashSet<u16>,
        buttons: HashSet<u8>,
        /// Keys that were already down locally when we crossed; their
        /// release must also reach this computer or they would stick.
        pass_up: HashSet<u16>,
    },
}

impl Ctl {
    fn local() -> Ctl {
        Ctl::Local { buttons: 0, held: HashSet::new(), streak: 0 }
    }
}

/// One live connection, for the command-line program's messages.
pub struct LinkStatus {
    pub name: String,
    pub side: Side,
    pub fast_pointer: bool,
}

/// The doorway as fractions along each computer's edge.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Door {
    pub local: [f64; 2],
    pub remote: [f64; 2],
}

/// One paired computer as the settings window draws it.
#[derive(Serialize, Debug, Clone)]
pub struct PeerStatus {
    pub key: String,
    pub name: String,
    pub side: Side,
    pub offset: i32,
    pub mode: PortalMode,
    pub screen: Option<[u32; 2]>,
    pub connected: bool,
    pub fast_pointer: bool,
    pub door: Option<Door>,
}

/// Everything a settings window needs to draw the current situation.
#[derive(Serialize, Debug, Clone)]
pub struct Status {
    pub name: String,
    pub key: String,
    pub screen: [i32; 2],
    pub pairing: bool,
    pub paused: bool,
    /// Name of the computer our cursor is on right now, if not this one.
    pub controlling: Option<String>,
    pub peers: Vec<PeerStatus>,
    /// Recent file and picture transfers, newest first.
    pub transfers: Vec<TransferStatus>,
    /// This computer's full public key, as other computers know it.
    pub id: String,
    /// Key of the computer whose speakers play everything, if one is chosen.
    pub speakers: Option<String>,
    pub sound: SoundState,
}

/// The running engine. There is one per program; everything else holds an `Arc` to it.
pub struct Node {
    platform: Arc<dyn Platform>,
    store: Store,
    identity: Identity,
    name: String,
    config: Mutex<Config>,
    links: Mutex<HashMap<Key, Arc<Link>>>,
    sessions: Mutex<HashMap<u64, Arc<Link>>>,
    ctl: Mutex<Ctl>,
    out: mpsc::Sender<Out>,
    tcp_port: u16,
    udp_port: u16,
    pairing: Option<PairingOptions>,
    pairing_on: AtomicBool,
    pair_lock: Mutex<()>,
    declined: Mutex<HashSet<Key>>,
    last_clipboard: Mutex<Option<String>>,
    connect: Vec<String>,
    paused: AtomicBool,
    stopped: AtomicBool,
    clip: Mutex<Clip>,
    /// Private folder where copied files wait to be pasted.
    staging: PathBuf,
    /// The copied files currently offered on the clipboard, if any.
    last_staged: Mutex<Option<PathBuf>>,
    received_dir: PathBuf,
    carry_limit: AtomicU64,
    records: Mutex<Vec<Record>>,
    next_record: AtomicU32,
    audio_out: Mutex<AudioOut>,
    /// Held while sound routing is being changed, so changes do not overlap.
    audio_switch: Mutex<()>,
    drag: Mutex<DragWatch>,
    /// A paired computer runs a different version: worth checking for an
    /// update now rather than at the next regular time.
    update_hint: AtomicBool,
}

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn log(text: &str) {
    eprintln!("[screen-portals] {text}");
}

/// Names come from the network and get printed, so keep them tame.
fn clean_name(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| !c.is_control()).take(48).collect();
    if cleaned.trim().is_empty() {
        "unnamed".to_string()
    } else {
        cleaned.trim().to_string()
    }
}

impl Node {
    /// Bind sockets and start the background threads.
    pub fn start(platform: Arc<dyn Platform>, store: Store, options: Options) -> Result<Arc<Node>, String> {
        let identity = store.identity()?;
        let config = store.load_config()?;
        let listener = TcpListener::bind((options.bind, options.tcp_port))
            .or_else(|_| TcpListener::bind((options.bind, 0)))
            .map_err(|e| format!("cannot listen for connections: {e}"))?;
        let tcp_port = listener.local_addr().map_err(err)?.port();
        let udp = UdpSocket::bind((options.bind, 0)).map_err(|e| format!("cannot open pointer socket: {e}"))?;
        let udp_port = udp.local_addr().map_err(err)?.port();
        let (out, outbox) = mpsc::channel();
        let pairing_on = options.pairing.as_ref().map_or(false, |p| p.active);

        // Anything left in these folders belongs to a run that has ended.
        let staging = std::env::temp_dir().join("ScreenPortals").join(fingerprint(&identity.public));
        let _ = fs::remove_dir_all(&staging);
        let received_dir = options.received_dir.unwrap_or_else(crate::config::default_received_dir);
        for entry in fs::read_dir(&received_dir).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with(".incoming-") {
                let _ = fs::remove_dir_all(entry.path());
            }
        }

        let node = Arc::new(Node {
            platform,
            store,
            identity,
            name: clean_name(&options.name),
            config: Mutex::new(config),
            links: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            ctl: Mutex::new(Ctl::local()),
            out,
            tcp_port,
            udp_port,
            pairing: options.pairing,
            pairing_on: AtomicBool::new(pairing_on),
            pair_lock: Mutex::new(()),
            declined: Mutex::new(HashSet::new()),
            last_clipboard: Mutex::new(None),
            connect: options.connect,
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            clip: Mutex::new(Clip { local: Carry::Nothing, carried: HashSet::new(), stamp: 0, latest: 0 }),
            staging,
            last_staged: Mutex::new(None),
            received_dir,
            carry_limit: AtomicU64::new(transfer::DEFAULT_CARRY_LIMIT),
            records: Mutex::new(Vec::new()),
            next_record: AtomicU32::new(1),
            audio_out: Mutex::new(AudioOut { to: None, seq: 0, gate: SilenceGate::default() }),
            audio_switch: Mutex::new(()),
            drag: Mutex::new(DragWatch::default()),
            update_hint: AtomicBool::new(false),
        });

        let udp_out = udp.try_clone().map_err(err)?;
        thread::spawn(move || send_loop(outbox, udp_out));

        let n = node.clone();
        thread::spawn(move || {
            for tcp in listener.incoming().flatten() {
                let n = n.clone();
                thread::spawn(move || {
                    if let Err(e) = n.establish(tcp, false, None) {
                        if e != ALREADY_CONNECTED {
                            log(&format!("incoming connection closed: {e}"));
                        }
                    }
                });
            }
        });

        let n = node.clone();
        thread::spawn(move || n.datagram_loop(udp));

        let n = node.clone();
        thread::spawn(move || loop {
            thread::sleep(TICK);
            for link in n.all_links() {
                n.send(&link, Msg::Ping);
                if !link.udp_ok.load(Ordering::Relaxed) {
                    let _ = n.out.send(Out::Probe(link));
                }
            }
        });

        let n = node.clone();
        thread::spawn(move || n.dial_loop());

        if options.discovery {
            crate::discovery::spawn(node.clone(), options.bind);
        }
        Ok(node)
    }

    // ----- information -----

    /// The name other computers see.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// This computer's identity.
    pub fn public_key(&self) -> Key {
        self.identity.public
    }

    /// The port actually being listened on (the preferred one may have been taken).
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
    }

    /// True while this computer is willing to pair with a new one.
    pub fn is_pairing(&self) -> bool {
        self.pairing.is_some() && self.pairing_on.load(Ordering::Relaxed)
    }

    /// Whether this computer has a way to ask the user about new computers.
    pub fn can_pair(&self) -> bool {
        self.pairing.is_some()
    }

    /// Switch pairing mode on or off. Returns false if this computer was
    /// started without a way to ask the user.
    pub fn set_pairing(&self, on: bool) -> bool {
        if self.pairing.is_none() {
            return false;
        }
        if on {
            self.declined.lock().unwrap().clear();
        }
        self.pairing_on.store(on, Ordering::Relaxed);
        true
    }

    /// True while sharing is paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// While paused the cursor stays on this computer; connections and the
    /// clipboard carry on.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        if paused {
            self.come_home();
        }
    }

    /// Every paired computer, connected or not.
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.config.lock().unwrap().peers.clone()
    }

    /// The computers connected right now.
    pub fn connected(&self) -> Vec<LinkStatus> {
        self.all_links()
            .iter()
            .map(|l| LinkStatus {
                name: l.name.clone(),
                side: l.placement.lock().unwrap().side,
                fast_pointer: l.udp_ok.load(Ordering::Relaxed),
            })
            .collect()
    }

    fn my_size(&self) -> (i32, i32) {
        let screen = self.platform.screen();
        (screen.w, screen.h)
    }

    /// A snapshot of everything, for the settings window.
    pub fn status(&self) -> Status {
        let mine = self.my_size();
        let controlling = match &*self.ctl.lock().unwrap() {
            Ctl::Remote { link, .. } => Some(link.name.clone()),
            Ctl::Local { .. } => None,
        };
        let peers = self.peers();
        // Each of these takes and releases its own locks, so they are read
        // before the list of connections is held.
        let speakers = self.config.lock().unwrap().speakers.clone();
        let sound = self.sound_state();
        let transfers = self.transfers();
        let links = self.links.lock().unwrap();
        let peers = peers
            .into_iter()
            .map(|peer| {
                let link = peer.key_bytes().and_then(|k| links.get(&k));
                let door = peer.screen.and_then(|[w, h]| layout::portal(peer.placement(), mine, (w as i32, h as i32)));
                PeerStatus {
                    connected: link.is_some(),
                    fast_pointer: link.map_or(false, |l| l.udp_ok.load(Ordering::Relaxed)),
                    door: door.map(|p| Door { local: [p.local.0, p.local.1], remote: [p.remote.0, p.remote.1] }),
                    key: peer.key,
                    name: peer.name,
                    side: peer.side,
                    offset: peer.offset,
                    mode: peer.mode,
                    screen: peer.screen,
                }
            })
            .collect();
        Status {
            name: self.name.clone(),
            key: fingerprint(&self.identity.public),
            screen: [mine.0, mine.1],
            pairing: self.is_pairing(),
            paused: self.is_paused(),
            controlling,
            peers,
            transfers,
            id: to_hex(&self.identity.public),
            speakers,
            sound,
        }
    }

    /// True while this computer's mouse and keyboard are driving another one.
    pub fn is_remote(&self) -> bool {
        matches!(*self.ctl.lock().unwrap(), Ctl::Remote { .. })
    }

    /// Stop for good: close every connection and refuse new ones.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        // The cursor comes home without a goodbye: the other computer lets
        // go of everything when the connection closes, and treats a held
        // drag as abandoned. A goodbye sent now would race the closing
        // and could arrive as an ordinary release, dropping the drag.
        *self.ctl.lock().unwrap() = Ctl::local();
        self.disconnect_all();
        // Give this computer its own sound back before the program exits.
        self.audio_reconcile();
    }

    /// Close every connection (they are re-established automatically).
    pub fn disconnect_all(&self) {
        for link in self.all_links() {
            link.stream.shutdown();
        }
    }

    fn all_links(&self) -> Vec<Arc<Link>> {
        self.links.lock().unwrap().values().cloned().collect()
    }

    /// Which computer, if any, lies beyond this point on this edge?
    fn door_at(&self, side: Side, t: f64) -> Option<(Arc<Link>, Portal)> {
        self.links.lock().unwrap().values().find_map(|link| {
            let portal = (*link.portal.lock().unwrap())?;
            (portal.side == side && portal.admits_local(t)).then(|| (link.clone(), portal))
        })
    }

    fn send(&self, link: &Arc<Link>, message: Msg) {
        let _ = self.out.send(Out::Tcp(link.clone(), message));
    }

    // ----- arranging -----

    /// Arrange a paired computer against this one and tell it, so that both
    /// agree where the doorway is.
    pub fn set_placement(&self, key: &Key, placement: Placement) -> Result<(), String> {
        self.apply_placement(key, placement, true)
    }

    fn apply_placement(&self, key: &Key, mut placement: Placement, notify: bool) -> Result<(), String> {
        let mine = self.my_size();
        {
            let mut config = self.config.lock().unwrap();
            let hex = to_hex(key);
            let peer = config.peers.iter_mut().find(|p| p.key == hex).ok_or("that computer is not paired")?;
            if let Some([w, h]) = peer.screen {
                placement.offset = layout::clamp_offset(placement.side, placement.offset, mine, (w as i32, h as i32));
            }
            peer.side = placement.side;
            peer.offset = placement.offset;
            peer.mode = placement.mode;
            self.store.save_config(&config)?;
        }
        let link = self.links.lock().unwrap().get(key).cloned();
        if let Some(link) = link {
            *link.placement.lock().unwrap() = placement;
            *link.portal.lock().unwrap() = layout::portal(placement, mine, (link.screen_w, link.screen_h));
            if notify {
                self.send(&link, Msg::Layout { side: placement.side, offset: placement.offset, mode: placement.mode });
            }
        }
        Ok(())
    }

    /// Remove a pairing. The computer can no longer connect.
    pub fn forget_key(&self, key: &Key) -> Result<bool, String> {
        let removed = {
            let mut config = self.config.lock().unwrap();
            let hex = to_hex(key);
            let before = config.peers.len();
            config.peers.retain(|p| p.key != hex);
            let removed = config.peers.len() != before;
            if removed {
                if config.speakers.as_deref() == Some(hex.as_str()) {
                    config.speakers = None;
                    config.speakers_rev += 1;
                }
                self.store.save_config(&config)?;
            }
            removed
        };
        let link = self.links.lock().unwrap().get(key).cloned();
        if let Some(link) = link {
            link.stream.shutdown();
        }
        Ok(removed)
    }

    // ----- connecting -----

    /// Connect to an address the user typed. It is remembered if this
    /// results in a new pairing.
    pub fn dial(self: &Arc<Self>, addr: SocketAddr) -> Result<String, String> {
        self.dial_inner(addr, true)
    }

    fn dial_inner(self: &Arc<Self>, addr: SocketAddr, remember: bool) -> Result<String, String> {
        let tcp = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| format!("cannot reach {addr}: {e}"))?;
        self.establish(tcp, true, remember.then_some(addr))
    }

    /// Called by discovery when another Screen Portals computer announces itself.
    pub fn on_beacon(self: &Arc<Self>, key: Key, name: &str, addr: SocketAddr, wants_pairing: bool) {
        if self.stopped.load(Ordering::Relaxed) || key == self.identity.public || self.links.lock().unwrap().contains_key(&key) {
            return;
        }
        // Only the side with the lower key dials, so two computers that see
        // each other at the same moment do not open two connections.
        if self.identity.public > key {
            return;
        }
        let known = self.config.lock().unwrap().find(&key).is_some();
        let pair_now = !known && wants_pairing && self.is_pairing() && !self.declined.lock().unwrap().contains(&key);
        if !known && !pair_now {
            return;
        }
        if pair_now {
            log(&format!("found \"{}\" in pairing mode, connecting", clean_name(name)));
        }
        if let Err(e) = self.dial_inner(addr, false) {
            if e != ALREADY_CONNECTED && pair_now {
                log(&format!("pairing with \"{}\" did not complete: {e}", clean_name(name)));
            }
        }
    }

    /// Keeps trying fixed addresses: ones given on the command line and ones
    /// saved with a pairing.
    fn dial_loop(self: Arc<Self>) {
        let mut last_error: HashMap<String, String> = HashMap::new();
        while !self.stopped.load(Ordering::Relaxed) {
            let mut targets: Vec<String> = Vec::new();
            {
                let config = self.config.lock().unwrap();
                let links = self.links.lock().unwrap();
                let missing = config.peers.iter().any(|p| p.key_bytes().map_or(false, |k| !links.contains_key(&k)));
                for peer in &config.peers {
                    let connected = peer.key_bytes().map_or(true, |k| links.contains_key(&k));
                    if let (false, Some(addr)) = (connected, &peer.addr) {
                        targets.push(addr.clone());
                    }
                }
                if missing || (self.is_pairing() && links.is_empty()) {
                    targets.extend(self.connect.iter().cloned());
                }
            }
            targets.sort();
            targets.dedup();
            for target in targets {
                let result = target
                    .to_socket_addrs()
                    .map_err(|e| format!("cannot look up {target}: {e}"))
                    .and_then(|mut addrs| addrs.next().ok_or_else(|| format!("cannot look up {target}")))
                    .and_then(|addr| self.dial_inner(addr, true));
                match result {
                    Ok(_) => {
                        last_error.remove(&target);
                    }
                    Err(e) if e == ALREADY_CONNECTED => {}
                    Err(e) => {
                        if last_error.get(&target) != Some(&e) {
                            log(&e);
                            last_error.insert(target, e);
                        }
                    }
                }
            }
            thread::sleep(Duration::from_secs(3));
        }
    }

    /// Authenticate a new TCP connection and, if all is well, start using it.
    fn establish(self: &Arc<Self>, tcp: TcpStream, initiator: bool, dialed: Option<SocketAddr>) -> Result<String, String> {
        if self.stopped.load(Ordering::Relaxed) {
            return Err("shutting down".into());
        }
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = tcp.set_write_timeout(Some(WRITE_TIMEOUT));
        let peer_ip = tcp.peer_addr().map_err(err)?.ip();
        let Established { stream, remote_key, hash } = secure::handshake(tcp, initiator, &self.identity.private)?;
        self.admit(stream, remote_key, &hash, initiator, peer_ip, dialed)
    }

    fn admit(
        self: &Arc<Self>,
        stream: SecureStream,
        remote_key: Key,
        hash: &[u8],
        initiator: bool,
        peer_ip: IpAddr,
        dialed: Option<SocketAddr>,
    ) -> Result<String, String> {
        let fail = |stream: &SecureStream, message: String| -> String {
            stream.shutdown();
            message
        };
        if remote_key == self.identity.public {
            return Err(fail(&stream, "that address is this computer".into()));
        }
        let known = self.config.lock().unwrap().find(&remote_key).cloned();
        if known.is_none() && !self.is_pairing() {
            return Err(fail(&stream, format!("refused unpaired device {}", fingerprint(&remote_key))));
        }

        let udp_key: [u8; 32] = random_bytes();
        let session = u64::from_le_bytes(random_bytes());
        let mine = self.my_size();
        stream.send(&Msg::Hello {
            version: PROTOCOL_VERSION,
            name: self.name.clone(),
            screen_w: mine.0.max(0) as u32,
            screen_h: mine.1.max(0) as u32,
            udp_port: self.udp_port,
            udp_key,
            udp_session: session,
            pairing: known.is_none(),
        })?;
        let hello = stream
            .recv()
            .map_err(|_| "the other computer closed the connection (if it is new, put both in pairing mode)".to_string())?;
        let Msg::Hello { version, name, screen_w, screen_h, udp_port, udp_key: peer_udp_key, udp_session: peer_session, pairing: peer_pairing } =
            hello
        else {
            return Err(fail(&stream, "unexpected first message".into()));
        };
        let peer_name = clean_name(&name);
        if version != PROTOCOL_VERSION {
            // One of the two is out of date; if it is us, an update fixes it.
            self.update_hint.store(true, Ordering::Relaxed);
            return Err(fail(&stream, format!("\"{peer_name}\" runs a different version of Screen Portals")));
        }

        let mut info = match known {
            Some(info) => {
                if peer_pairing {
                    return Err(fail(
                        &stream,
                        format!("\"{peer_name}\" no longer remembers this computer; forget it here and pair again"),
                    ));
                }
                info
            }
            None => {
                if !peer_pairing {
                    return Err(fail(&stream, format!("\"{peer_name}\" is not in pairing mode")));
                }
                match self.pair(&stream, hash, initiator, &remote_key, &peer_name, dialed) {
                    Ok(info) => info,
                    Err(e) => return Err(fail(&stream, e)),
                }
            }
        };

        // Remember its screen size so it can be arranged while offline.
        let theirs = ((screen_w as i32).max(2), (screen_h as i32).max(2));
        let seen = Some([theirs.0 as u32, theirs.1 as u32]);
        let clamped = layout::clamp_offset(info.side, info.offset, mine, theirs);
        if info.screen != seen || info.offset != clamped {
            info.screen = seen;
            info.offset = clamped;
            let mut config = self.config.lock().unwrap();
            config.upsert(info.clone());
            if let Err(e) = self.store.save_config(&config) {
                log(&e);
            }
        }
        let placement = info.placement();

        stream.set_timeouts(Some(IDLE_TIMEOUT), Some(WRITE_TIMEOUT));
        let by_lower = if initiator { self.identity.public < remote_key } else { remote_key < self.identity.public };
        let link = Arc::new(Link {
            key: remote_key,
            name: info.name.clone(),
            placement: Mutex::new(placement),
            portal: Mutex::new(layout::portal(placement, mine, theirs)),
            stream,
            screen_w: theirs.0,
            screen_h: theirs.1,
            sealer: Sealer::new(&udp_key, session),
            opener: Opener::new(&peer_udp_key, peer_session),
            peer_session,
            udp_addr: SocketAddr::new(peer_ip, udp_port),
            udp_ok: AtomicBool::new(false),
            probe_acked: AtomicBool::new(false),
            by_lower,
            injected_keys: Mutex::new(HashSet::new()),
            injected_buttons: Mutex::new(HashSet::new()),
            incoming: Mutex::new(HashMap::new()),
            outgoing: Mutex::new(HashMap::new()),
            visiting: AtomicBool::new(false),
            drag_hold: Mutex::new(Hold::None),
        });

        {
            let mut links = self.links.lock().unwrap();
            if let Some(old) = links.get(&remote_key) {
                if by_lower && !old.by_lower {
                    old.stream.shutdown();
                } else {
                    link.stream.shutdown();
                    return Err(ALREADY_CONNECTED.into());
                }
            }
            links.insert(remote_key, link.clone());
        }
        self.sessions.lock().unwrap().insert(peer_session, link.clone());
        log(&format!("connected to \"{}\" ({} this computer)", link.name, placement.side.phrase()));
        let _ = self.out.send(Out::Probe(link.clone()));
        {
            // Compare notes on where sound should play; the newer choice wins.
            let config = self.config.lock().unwrap();
            if config.speakers_rev > 0 {
                let key = config.speakers.as_deref().and_then(from_hex).and_then(|k| k.try_into().ok());
                self.send(&link, Msg::Speakers { key, rev: config.speakers_rev });
            }
        }

        let node = self.clone();
        let reader = link.clone();
        thread::spawn(move || {
            while let Ok(message) = reader.stream.recv() {
                node.handle(&reader, message);
            }
            node.drop_link(&reader);
        });
        Ok(info.name)
    }

    /// First contact between two computers: agree on a code the user checks.
    fn pair(
        &self,
        stream: &SecureStream,
        hash: &[u8],
        initiator: bool,
        key: &Key,
        peer_name: &str,
        dialed: Option<SocketAddr>,
    ) -> Result<PeerInfo, String> {
        let options = self.pairing.as_ref().ok_or("not in pairing mode")?;
        if self.declined.lock().unwrap().contains(key) {
            return Err("pairing with this device was declined earlier".into());
        }
        let _one_at_a_time = self.pair_lock.lock().unwrap();
        let unexpected = || "pairing was interrupted".to_string();

        let (dialer_nonce, listener_nonce) = if initiator {
            let mine: [u8; 16] = random_bytes();
            stream.send(&Msg::PairCommit(pairing::commitment(hash, &mine)))?;
            let Msg::PairNonce(theirs) = stream.recv()? else { return Err(unexpected()) };
            stream.send(&Msg::PairReveal(mine))?;
            (mine, theirs)
        } else {
            let Msg::PairCommit(commit) = stream.recv()? else { return Err(unexpected()) };
            let mine: [u8; 16] = random_bytes();
            stream.send(&Msg::PairNonce(mine))?;
            let Msg::PairReveal(theirs) = stream.recv()? else { return Err(unexpected()) };
            if pairing::commitment(hash, &theirs) != commit {
                return Err("pairing safety check failed; someone may be interfering with the connection".into());
            }
            (theirs, mine)
        };
        let code = pairing::code(hash, &dialer_nonce, &listener_nonce);

        stream.send(&Msg::PairSide(options.side))?;
        let Msg::PairSide(their_view) = stream.recv()? else { return Err(unexpected()) };
        // If nobody said where the computers sit, start with a consistent
        // guess (dialler on the left); it is rearranged in the layout later.
        let side = options
            .side
            .or(their_view.map(Side::opposite))
            .unwrap_or(if initiator { Side::Right } else { Side::Left });

        stream.set_timeouts(Some(PAIR_TIMEOUT), Some(WRITE_TIMEOUT));
        let accepted = (options.confirm)(peer_name, &code);
        stream.send(&Msg::PairConfirm(accepted))?;
        if !accepted {
            self.declined.lock().unwrap().insert(*key);
            return Err("pairing declined on this computer".into());
        }
        match stream.recv() {
            Ok(Msg::PairConfirm(true)) => {}
            Ok(Msg::PairConfirm(false)) => return Err("pairing declined on the other computer".into()),
            _ => return Err("the other computer did not confirm in time".into()),
        }

        let mut info = PeerInfo::new(key, peer_name, side);
        info.addr = dialed.map(|a| a.to_string());
        let mut config = self.config.lock().unwrap();
        config.upsert(info.clone());
        self.store.save_config(&config)?;
        self.pairing_on.store(false, Ordering::Relaxed);
        log(&format!("paired with \"{peer_name}\""));
        Ok(info)
    }

    fn drop_link(&self, link: &Arc<Link>) {
        {
            let mut links = self.links.lock().unwrap();
            if links.get(&link.key).map_or(false, |l| Arc::ptr_eq(l, link)) {
                links.remove(&link.key);
            }
        }
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.get(&link.peer_session).map_or(false, |l| Arc::ptr_eq(l, link)) {
                sessions.remove(&link.peer_session);
            }
        }
        link.stream.shutdown();
        self.release_injected(link);

        // Transfers in either direction end here.
        for out in link.outgoing.lock().unwrap().values() {
            let _ = out.stop.compare_exchange(RUNNING, LINK_LOST, Ordering::Relaxed, Ordering::Relaxed);
            out.moved.notify_all();
        }
        let cut_short: Vec<Incoming> = link.incoming.lock().unwrap().drain().map(|(_, incoming)| incoming).collect();
        for incoming in cut_short {
            self.close_record(incoming.record, TransferState::Failed, Some("The connection was lost".into()));
        }

        // If our sound was playing over there, it plays here again.
        self.audio_reconcile();

        // If our own cursor was over there, bring it home.
        let mut warp = None;
        {
            let mut ctl = self.ctl.lock().unwrap();
            let side = match &*ctl {
                Ctl::Remote { link: active, pos, .. } if Arc::ptr_eq(active, link) => Some(pos.side),
                _ => None,
            };
            if let Some(side) = side {
                *ctl = Ctl::local();
                warp = Some(layout::return_point(self.platform.screen(), side, 0.5));
            }
        }
        if let Some((x, y)) = warp {
            self.platform.warp(x, y);
        }
        log(&format!("disconnected from \"{}\"", link.name));
    }

    /// Let go of anything a remote computer was holding down here.
    fn release_injected(&self, link: &Link) {
        let keys: Vec<u16> = link.injected_keys.lock().unwrap().drain().collect();
        for hid in keys {
            self.platform.inject_key(hid, false);
        }
        let buttons: Vec<u8> = link.injected_buttons.lock().unwrap().drain().collect();
        if buttons.contains(&BUTTON_LEFT) {
            // If that button was holding a drag, abandon the drag rather
            // than dropping it on whatever happens to be under the cursor.
            self.platform.drag_cancel();
        }
        for button in buttons {
            self.platform.inject_button(button, false);
        }
    }

    // ----- input arriving from another computer -----

    fn handle(self: &Arc<Self>, link: &Arc<Link>, message: Msg) {
        match message {
            Msg::Enter { x, y } => {
                link.visiting.store(true, Ordering::Relaxed);
                self.platform.inject_abs(x, y);
            }
            Msg::MouseAbs { x, y } => self.platform.inject_abs(x, y),
            Msg::Button { button, down, x, y } => {
                self.platform.inject_abs(x, y);
                let pressed_here = {
                    let mut held = link.injected_buttons.lock().unwrap();
                    if down {
                        held.insert(button);
                        true
                    } else {
                        held.remove(&button)
                    }
                };
                if pressed_here {
                    self.platform.inject_button(button, down);
                } else if button == BUTTON_LEFT {
                    // The button that crossed over holding dragged files
                    // was let go before the files arrived.
                    let mut hold = link.drag_hold.lock().unwrap();
                    if *hold == Hold::Holding {
                        *hold = Hold::Released;
                    }
                }
            }
            Msg::DragText(text) => {
                if !text.is_empty() && text.len() <= MAX_DRAG_TEXT {
                    self.drag_arrived(link, DragPayload::Text(text));
                }
            }
            Msg::DragUrl(url) => {
                if let Some(url) = transfer::safe_url(&url) {
                    self.drag_arrived(link, DragPayload::Url(url));
                }
            }
            Msg::DragCatch => {
                // Only meaningful if that computer was holding the button here.
                if link.injected_buttons.lock().unwrap().remove(&BUTTON_LEFT) {
                    self.drag_look(Purpose::Return { link: link.clone() });
                }
            }
            Msg::Open { url } => {
                if let Some(url) = transfer::safe_url(&url) {
                    log(&format!("opening a page sent by \"{}\"", link.name));
                    self.platform.open_url(&url);
                }
            }
            Msg::Wheel { dx, dy } => self.platform.inject_wheel(dx, dy),
            Msg::Key { hid, down } => {
                {
                    let mut held = link.injected_keys.lock().unwrap();
                    if down {
                        held.insert(hid);
                    } else {
                        held.remove(&hid);
                    }
                }
                self.platform.inject_key(hid, down);
            }
            Msg::Leave => {
                link.visiting.store(false, Ordering::Relaxed);
                self.release_injected(link);
                // The cursor is going back: anything copied here goes with it.
                self.carry(link);
            }
            Msg::ClipboardText(text) => {
                if text.len() <= MAX_CLIPBOARD {
                    *self.last_clipboard.lock().unwrap() = Some(text.clone());
                    self.clip.lock().unwrap().changed(Carry::Nothing);
                    self.platform.set_clipboard_text(&text);
                }
            }
            Msg::TransferBegin { id, kind, label, files, entries, bytes } => {
                if kind == TransferKind::Drag {
                    let held = link.visiting.load(Ordering::Relaxed) || self.left_held_here();
                    *link.drag_hold.lock().unwrap() = if held { Hold::Holding } else { Hold::Released };
                }
                self.incoming_begin(link, id, kind, &label, files, entries, bytes)
            }
            Msg::TransferEntry { id, path, size, dir } => {
                self.incoming_step(link, id, |incoming| match incoming.files.as_mut() {
                    Some(receiver) => receiver.entry(&path, size, dir).map(|_| None),
                    None => Err("a picture cannot contain files".into()),
                });
            }
            Msg::TransferData { id, data } => {
                self.incoming_step(link, id, |incoming| match incoming.files.as_mut() {
                    Some(receiver) => receiver.data(&data).map(Some),
                    None if incoming.image.len() as u64 + data.len() as u64 > incoming.bytes => {
                        Err("more data than announced".into())
                    }
                    None => {
                        incoming.image.extend_from_slice(&data);
                        Ok(Some(incoming.image.len() as u64))
                    }
                });
            }
            Msg::TransferEnd { id } => {
                let ours = link.outgoing.lock().unwrap().get(&id).cloned();
                match ours {
                    Some(out) => {
                        out.progress.lock().unwrap().1 = true;
                        out.moved.notify_all();
                    }
                    None => self.incoming_end(link, id),
                }
            }
            Msg::TransferAck { id, bytes } => {
                if let Some(out) = link.outgoing.lock().unwrap().get(&id) {
                    let mut progress = out.progress.lock().unwrap();
                    progress.0 = progress.0.max(bytes);
                    out.moved.notify_all();
                }
            }
            Msg::TransferCancel { id } => {
                if let Some(out) = link.outgoing.lock().unwrap().get(&id) {
                    let _ = out.stop.compare_exchange(RUNNING, BY_PEER, Ordering::Relaxed, Ordering::Relaxed);
                    out.moved.notify_all();
                }
                let stopped = link.incoming.lock().unwrap().remove(&id);
                if let Some(incoming) = stopped {
                    self.close_record(incoming.record, TransferState::Cancelled, Some("Stopped on the other computer".into()));
                }
            }
            Msg::UdpOk => {
                link.udp_ok.store(true, Ordering::Relaxed);
                self.audio_reconcile();
            }
            Msg::Speakers { key, rev } => self.speakers_heard(key, rev),
            Msg::Layout { side, offset, mode } => {
                // The other computer rearranged us; adopt the mirror image.
                let theirs = Placement { side, offset, mode };
                if let Err(e) = self.apply_placement(&link.key, theirs.mirrored(), false) {
                    log(&e);
                }
            }
            _ => {}
        }
    }

    fn datagram_loop(self: Arc<Self>, udp: UdpSocket) {
        let mut buf = [0u8; 1500];
        loop {
            let Ok((n, _from)) = udp.recv_from(&mut buf) else {
                // Windows reports ICMP "port unreachable" from earlier sends
                // as a receive error; it is harmless.
                thread::sleep(Duration::from_millis(1));
                continue;
            };
            let packet = &buf[..n];
            let Some(session) = datagram::session_of(packet) else { continue };
            let Some(link) = self.sessions.lock().unwrap().get(&session).cloned() else { continue };
            match link.opener.open(packet) {
                Some(Dgram::Probe) => {
                    if !link.probe_acked.swap(true, Ordering::Relaxed) {
                        self.send(&link, Msg::UdpOk);
                    }
                }
                Some(Dgram::MouseAbs { x, y }) => self.platform.inject_abs(x, y),
                Some(Dgram::Audio { seq, rate, pcm }) => {
                    // A computer whose own sound is being sent away never
                    // plays what it receives, so sound cannot go in circles.
                    if self.audio_out.lock().unwrap().to.is_none() {
                        self.platform.play_audio(link.peer_session, seq, rate, &audio::pcm_from_bytes(&pcm));
                    }
                }
                None => {}
            }
        }
    }

    // ----- input from this computer's own mouse and keyboard -----
    // Each returns true when the event must be hidden from this computer
    // because it now belongs to the other one.

    /// The mouse moved to this position (which may lie just outside the desktop).
    pub fn on_mouse_move(self: &Arc<Self>, px: i32, py: i32) -> bool {
        let screen = self.platform.screen();
        let mut warp = None;
        let swallow = {
            let mut ctl = self.ctl.lock().unwrap();
            let mut next = None;
            let mut returning = false;
            let swallow = match &mut *ctl {
                Ctl::Local { buttons, held, streak } => {
                    // A held left button may be a drag, which can cross too.
                    let dragging = *buttons == LEFT_HELD;
                    let hit = if (*buttons == 0 || dragging) && !self.is_paused() { layout::edge_hit(screen, px, py) } else { None };
                    let Some((side, beyond)) = hit else {
                        *streak = 0;
                        return false;
                    };
                    *streak = streak.saturating_add(1);
                    // Cross when pushing past the edge, or resting on it for
                    // a second event in case the system clamps coordinates.
                    let t = layout::edge_fraction(screen, side, px, py);
                    let target = if beyond || *streak >= 2 { self.door_at(side, t) } else { None };
                    let Some((link, portal)) = target else { return false };
                    if dragging {
                        // Ask the platform what, if anything, is being
                        // dragged. The cursor stays here until it answers.
                        self.drag_look_once(Purpose::Cross { link, portal, side, t });
                        return false;
                    }

                    let pos = Remote::enter(side, link.screen_w, link.screen_h, portal.to_remote(t), portal.remote);
                    let (x, y) = pos.normalized();
                    self.send(&link, Msg::Enter { x, y });
                    // Copied files and pictures follow the cursor across.
                    self.carry(&link);
                    let pass_up = std::mem::take(held);
                    let mut keys = HashSet::new();
                    for &hid in pass_up.iter().filter(|hid| keymap::is_modifier(**hid)) {
                        self.send(&link, Msg::Key { hid, down: true });
                        keys.insert(hid);
                    }
                    next = Some(Ctl::Remote { link, pos, portal, keys, buttons: HashSet::new(), pass_up });
                    warp = Some(self.platform.park_point());
                    true
                }
                Ctl::Remote { link, pos, portal, buttons, .. } => {
                    // The local cursor is parked and never actually moves, so
                    // the distance from it is exactly how far the mouse went.
                    let (cx, cy) = self.platform.cursor();
                    let (dx, dy) = (px - cx, py - cy);
                    if dx != 0 || dy != 0 {
                        match pos.apply(dx, dy) {
                            None => {
                                let (x, y) = pos.normalized();
                                let _ = self.out.send(Out::Mouse(link.clone(), x, y));
                            }
                            Some(t) => {
                                warp = Some(layout::return_point(screen, pos.side, portal.to_local(t)));
                                // Coming home with the button still down:
                                // remember that it is, so the hold is not
                                // mistaken for a fresh one.
                                let still = if buttons.contains(&BUTTON_LEFT) { LEFT_HELD } else { 0 };
                                next = Some(Ctl::Local { buttons: still, held: HashSet::new(), streak: 0 });
                                returning = true;
                            }
                        }
                    }
                    true
                }
            };
            if let Some(next) = next {
                let previous = std::mem::replace(&mut *ctl, next);
                self.let_go_with(previous, returning);
            }
            swallow
        };
        if let Some((x, y)) = warp {
            self.platform.warp(x, y);
        }
        swallow
    }

    /// Leaving the other computer: release everything we were holding there.
    fn let_go(&self, previous: Ctl) {
        self.let_go_with(previous, false);
    }

    /// `through_edge` is true when the cursor came back through the doorway
    /// (rather than by the escape keys or a pause). Then a held left button
    /// may be carrying a drag, which the other computer is asked to hand
    /// over instead of dropping where it is.
    fn let_go_with(&self, previous: Ctl, through_edge: bool) {
        if let Ctl::Remote { link, pos, keys, buttons, .. } = previous {
            let (x, y) = pos.normalized();
            for hid in keys {
                self.send(&link, Msg::Key { hid, down: false });
            }
            for button in buttons {
                if through_edge && button == BUTTON_LEFT {
                    self.send(&link, Msg::DragCatch);
                } else {
                    self.send(&link, Msg::Button { button, down: false, x, y });
                }
            }
            self.send(&link, Msg::Leave);
        }
    }

    /// A mouse button went down or up.
    pub fn on_button(&self, button: u8, down: bool) -> bool {
        let mut ctl = self.ctl.lock().unwrap();
        match &mut *ctl {
            Ctl::Local { buttons, .. } => {
                let bit = 1u8.checked_shl(button as u32).unwrap_or(0);
                if down {
                    *buttons |= bit;
                } else {
                    *buttons &= !bit;
                }
                if button == BUTTON_LEFT && !down {
                    // The next press is a new drag, and files still on their
                    // way here for this one have nothing to be dropped by.
                    let mut drag = self.drag.lock().unwrap();
                    drag.tried = false;
                    if matches!(drag.waiting, Some((_, Purpose::Cross { .. }))) {
                        drag.waiting = None;
                    }
                    drop(drag);
                    for link in self.all_links() {
                        let mut hold = link.drag_hold.lock().unwrap();
                        if *hold == Hold::Holding && !link.visiting.load(Ordering::Relaxed) {
                            *hold = Hold::Released;
                        }
                    }
                }
                false
            }
            Ctl::Remote { link, pos, buttons, .. } => {
                if down {
                    buttons.insert(button);
                } else {
                    buttons.remove(&button);
                }
                let (x, y) = pos.normalized();
                self.send(link, Msg::Button { button, down, x, y });
                true
            }
        }
    }

    /// The wheel turned.
    pub fn on_wheel(&self, dx: i16, dy: i16) -> bool {
        match &*self.ctl.lock().unwrap() {
            Ctl::Local { .. } => false,
            Ctl::Remote { link, .. } => {
                self.send(link, Msg::Wheel { dx, dy });
                true
            }
        }
    }

    /// A key went down or up. Also watches for the Ctrl+Alt+Home escape.
    pub fn on_key(&self, hid: u16, down: bool) -> bool {
        let mut ctl = self.ctl.lock().unwrap();
        match &mut *ctl {
            Ctl::Local { held, .. } => {
                if down {
                    held.insert(hid);
                } else {
                    held.remove(&hid);
                }
                return false;
            }
            Ctl::Remote { link, keys, pass_up, .. } => {
                if !down {
                    let started_here = pass_up.remove(&hid);
                    if keys.remove(&hid) {
                        self.send(link, Msg::Key { hid, down: false });
                    }
                    return !started_here;
                }
                // Ctrl+Alt+Home always brings the cursor back.
                let escape = hid == keymap::HID_HOME
                    && keys.iter().any(|k| keymap::is_ctrl(*k))
                    && keys.iter().any(|k| keymap::is_alt(*k));
                if !escape {
                    keys.insert(hid);
                    self.send(link, Msg::Key { hid, down: true });
                    return true;
                }
            }
        }
        let previous = std::mem::replace(&mut *ctl, Ctl::local());
        self.let_go(previous);
        true
    }

    /// Bring the cursor back to this computer if it is elsewhere.
    pub fn come_home(&self) {
        let previous = std::mem::replace(&mut *self.ctl.lock().unwrap(), Ctl::local());
        self.let_go(previous);
    }

    /// The local clipboard now holds this text.
    pub fn on_clipboard_text(&self, text: String) {
        if text.is_empty() || text.len() > MAX_CLIPBOARD {
            return;
        }
        {
            let mut last = self.last_clipboard.lock().unwrap();
            if last.as_deref() == Some(text.as_str()) {
                return; // our own write (or nothing new) coming back to us
            }
            *last = Some(text.clone());
        }
        self.clip.lock().unwrap().changed(Carry::Nothing);
        for link in self.all_links() {
            self.send(&link, Msg::ClipboardText(text.clone()));
        }
    }

    /// Files or folders were copied on this computer. They are sent when
    /// the cursor next crosses to another computer.
    pub fn on_clipboard_files(&self, paths: Vec<PathBuf>) {
        *self.last_clipboard.lock().unwrap() = None;
        let local = if paths.is_empty() { Carry::Nothing } else { Carry::Files(paths) };
        self.clip.lock().unwrap().changed(local);
    }

    /// A picture was copied on this computer. It is read from the platform
    /// and sent when the cursor next crosses to another computer.
    pub fn on_clipboard_image(&self) {
        *self.last_clipboard.lock().unwrap() = None;
        self.clip.lock().unwrap().changed(Carry::Image);
    }

    /// The clipboard now holds something that cannot be shared.
    pub fn on_clipboard_cleared(&self) {
        *self.last_clipboard.lock().unwrap() = None;
        self.clip.lock().unwrap().changed(Carry::Nothing);
    }
}

// ----- updates -----

impl Node {
    /// True once after a paired computer turned out to run a different
    /// version of the program.
    pub fn take_update_hint(&self) -> bool {
        self.update_hint.swap(false, Ordering::Relaxed)
    }

    /// Nothing is going on that a restart would interrupt: the cursor is
    /// at home, nobody else's cursor is here, and no files are on the move.
    pub fn is_quiet(&self) -> bool {
        let visited = self.all_links().iter().any(|link| link.visiting.load(Ordering::Relaxed));
        let moving = self.records.lock().unwrap().iter().any(|r| r.status.state == TransferState::Active);
        !self.is_remote() && !visited && !moving
    }
}

// ----- dragging across -----

impl Node {
    fn left_held_here(&self) -> bool {
        matches!(&*self.ctl.lock().unwrap(), Ctl::Local { buttons, .. } if buttons & LEFT_HELD != 0)
    }

    /// Ask the platform whether something is being dragged at the cursor.
    fn drag_look(&self, purpose: Purpose) {
        let token = {
            let mut drag = self.drag.lock().unwrap();
            drag.next_token += 1;
            drag.waiting = Some((drag.next_token, purpose));
            drag.next_token
        };
        self.platform.drag_catch(token);
    }

    /// The same, but only once for each press of the button, so resting a
    /// held button on the edge does not ask over and over.
    fn drag_look_once(&self, purpose: Purpose) {
        {
            let mut drag = self.drag.lock().unwrap();
            if drag.tried {
                return;
            }
            drag.tried = true;
        }
        self.drag_look(purpose);
    }

    /// The platform's answer to `Platform::drag_catch`. By now the drag, if
    /// there was one, has ended on this computer and we hold what it carried.
    pub fn on_drag_caught(self: &Arc<Self>, token: u64, caught: Caught) {
        let purpose = {
            let mut drag = self.drag.lock().unwrap();
            match drag.waiting.take() {
                Some((waiting, purpose)) if waiting == token => purpose,
                other => {
                    drag.waiting = other;
                    return;
                }
            }
        };
        match (purpose, caught) {
            (Purpose::Cross { link, portal, side, t }, Caught::Data(payload)) => self.cross_with_drag(link, portal, side, t, payload),
            // A browser window pushed against the doorway: its page opens
            // over there. The window itself, and the cursor, stay here.
            (Purpose::Cross { link, .. }, Caught::Page(url)) => self.send(&link, Msg::Open { url }),
            (Purpose::Cross { .. }, Caught::Nothing) => {}
            (Purpose::Return { link }, Caught::Data(payload)) => self.send_drag(&link, payload),
            // Nothing was being dragged after all: just let the button go.
            (Purpose::Return { .. }, _) => self.platform.inject_button(BUTTON_LEFT, false),
        }
    }

    /// Move our cursor through the doorway together with what it dragged.
    fn cross_with_drag(self: &Arc<Self>, link: Arc<Link>, portal: Portal, side: Side, t: f64, payload: DragPayload) {
        {
            let mut ctl = self.ctl.lock().unwrap();
            let still_held = matches!(&*ctl, Ctl::Local { buttons, .. } if *buttons == LEFT_HELD);
            if !still_held || !self.link_alive(&link) || self.is_paused() {
                drop(ctl);
                // The drag has already ended here, so do not lose it.
                self.keep_for_pasting(payload);
                return;
            }
            let Ctl::Local { held, .. } = &mut *ctl else { return };
            let pos = Remote::enter(side, link.screen_w, link.screen_h, portal.to_remote(t), portal.remote);
            let (x, y) = pos.normalized();
            self.send(&link, Msg::Enter { x, y });
            let pass_up = std::mem::take(held);
            let mut keys = HashSet::new();
            for &hid in pass_up.iter().filter(|hid| keymap::is_modifier(**hid)) {
                self.send(&link, Msg::Key { hid, down: true });
                keys.insert(hid);
            }
            // The button is still physically down, so it counts as held over
            // there; letting go of it will be forwarded like any release.
            let buttons = HashSet::from([BUTTON_LEFT]);
            *ctl = Ctl::Remote { link: link.clone(), pos, portal, keys, buttons, pass_up };
        }
        let (x, y) = self.platform.park_point();
        self.platform.warp(x, y);
        self.send_drag(&link, payload);
    }

    /// Send what was being dragged to the computer the drag continues on.
    fn send_drag(self: &Arc<Self>, link: &Arc<Link>, payload: DragPayload) {
        match payload {
            DragPayload::Text(text) => self.send(link, Msg::DragText(text)),
            DragPayload::Url(url) => self.send(link, Msg::DragUrl(url)),
            DragPayload::Files(paths) => {
                let (node, link) = (self.clone(), link.clone());
                thread::spawn(move || {
                    node.carry_files(&link, &paths, TransferKind::Drag);
                });
            }
        }
    }

    /// Something dragged from another computer has arrived. If the button
    /// is still down the drag carries on here; otherwise it is kept for
    /// pasting.
    fn drag_arrived(&self, link: &Arc<Link>, payload: DragPayload) {
        if link.visiting.load(Ordering::Relaxed) {
            // The other computer's cursor is here holding the button: press
            // it on its behalf, so its eventual release drops the drag.
            link.injected_buttons.lock().unwrap().insert(BUTTON_LEFT);
            self.platform.drag_begin(payload);
        } else if self.left_held_here() {
            self.platform.drag_begin(payload);
        } else {
            self.keep_for_pasting(payload);
        }
    }

    fn keep_for_pasting(&self, payload: DragPayload) {
        match payload {
            DragPayload::Text(text) | DragPayload::Url(text) => {
                *self.last_clipboard.lock().unwrap() = Some(text.clone());
                self.clip.lock().unwrap().changed(Carry::Nothing);
                self.platform.set_clipboard_text(&text);
            }
            DragPayload::Files(paths) => self.platform.set_clipboard_files(&paths),
        }
    }
}

// ----- sound -----

impl Node {
    /// Choose the computer whose speakers play everyone's sound: this one,
    /// a paired one, or `None` for each computer playing its own. Paired
    /// computers are told, so the choice only has to be made once.
    pub fn set_speakers(&self, key: Option<Key>) -> Result<(), String> {
        let rev = {
            let mut config = self.config.lock().unwrap();
            if let Some(key) = &key {
                if *key != self.identity.public && config.find(key).is_none() {
                    return Err("that computer is not paired".into());
                }
            }
            config.speakers = key.as_ref().map(|k| to_hex(k));
            config.speakers_rev += 1;
            self.store.save_config(&config)?;
            config.speakers_rev
        };
        for link in self.all_links() {
            self.send(&link, Msg::Speakers { key, rev });
        }
        self.audio_reconcile();
        Ok(())
    }

    /// The computer chosen to play everything, if any.
    pub fn speakers(&self) -> Option<Key> {
        self.config.lock().unwrap().speakers.as_deref().and_then(from_hex).and_then(|k| k.try_into().ok())
    }

    /// Another computer says where sound should play.
    fn speakers_heard(&self, key: Option<Key>, rev: u64) {
        {
            let mut config = self.config.lock().unwrap();
            if rev <= config.speakers_rev {
                return;
            }
            // A computer this one has never met cannot play its sound, but
            // the choice is still remembered so everyone agrees.
            config.speakers = key.as_ref().map(|k| to_hex(k));
            config.speakers_rev = rev;
            if let Err(e) = self.store.save_config(&config) {
                log(&e);
            }
        }
        self.audio_reconcile();
    }

    /// The connection our sound should be sent over right now, if any.
    fn audio_target(&self) -> Option<Arc<Link>> {
        if self.stopped.load(Ordering::Relaxed) {
            return None;
        }
        let key = self.speakers()?;
        if key == self.identity.public {
            return None;
        }
        let link = self.links.lock().unwrap().get(&key).cloned()?;
        // Sound only travels on the fast path; without it, it stays here.
        link.udp_ok.load(Ordering::Relaxed).then_some(link)
    }

    fn sound_state(&self) -> SoundState {
        if self.audio_out.lock().unwrap().to.is_some() {
            return SoundState::Sending;
        }
        match self.speakers() {
            Some(key) if key != self.identity.public => SoundState::Waiting,
            _ => SoundState::Own,
        }
    }

    /// Make what is actually happening match the choice and the
    /// connections: start or stop sending this computer's sound away.
    fn audio_reconcile(&self) {
        let _one_at_a_time = self.audio_switch.lock().unwrap();
        let want = self.audio_target();
        let (was, now) = {
            let mut out = self.audio_out.lock().unwrap();
            let same = match (&out.to, &want) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            };
            if same {
                return;
            }
            let was = out.to.is_some();
            out.gate = SilenceGate::default();
            out.to = want;
            (was, out.to.is_some())
        };
        if was != now {
            self.platform.capture_audio(now);
            log(if now { "this computer's sound now plays on the chosen computer" } else { "this computer plays its own sound again" });
        }
    }

    /// Sound this computer's programs just produced: 16-bit stereo at
    /// `rate`. Called by the platform while capturing; never blocks.
    pub fn on_audio_captured(&self, rate: u32, stereo: &[i16]) {
        let mut out = self.audio_out.lock().unwrap();
        let Some(link) = out.to.clone() else { return };
        if !out.gate.pass(stereo, rate) {
            return;
        }
        for chunk in stereo.chunks(audio::PACKET_FRAMES * 2) {
            out.seq = out.seq.wrapping_add(1);
            let sound = Dgram::Audio { seq: out.seq, rate, pcm: audio::pcm_bytes(chunk) };
            let _ = self.out.send(Out::Audio(link.clone(), sound));
        }
    }
}

// ----- files and pictures -----

impl Node {
    /// Recent transfers, newest first.
    pub fn transfers(&self) -> Vec<TransferStatus> {
        self.records.lock().unwrap().iter().rev().map(|r| r.status.clone()).collect()
    }

    /// Stop a transfer that is under way. Returns false if there is none.
    pub fn cancel_transfer(&self, id: u32) -> bool {
        let records = self.records.lock().unwrap();
        let Some(record) = records.iter().find(|r| r.status.id == id && r.status.state == TransferState::Active) else {
            return false;
        };
        let _ = record.stop.compare_exchange(RUNNING, BY_USER, Ordering::Relaxed, Ordering::Relaxed);
        true
    }

    /// Show a received file in its folder. Only paths this computer saved
    /// itself can be shown; the caller never supplies one.
    pub fn reveal_transfer(&self, id: u32) -> bool {
        let path = self.records.lock().unwrap().iter().find(|r| r.status.id == id).and_then(|r| r.reveal.clone());
        match path {
            Some(path) => {
                self.platform.reveal(&path);
                true
            }
            None => false,
        }
    }

    /// Copied files larger than this in total stay behind when the cursor
    /// crosses, and are refused when another computer tries to carry them here.
    pub fn set_carry_limit(&self, bytes: u64) {
        self.carry_limit.store(bytes, Ordering::Relaxed);
    }

    /// Where files sent to this computer are saved.
    pub fn received_dir(&self) -> &Path {
        &self.received_dir
    }

    fn open_record(&self, link: &Link, incoming: bool, kind: TransferKind, label: &str, files: u32, bytes: u64) -> (u32, Arc<AtomicU8>) {
        let id = self.next_record.fetch_add(1, Ordering::Relaxed);
        let stop = Arc::new(AtomicU8::new(RUNNING));
        let mut records = self.records.lock().unwrap();
        records.push(Record {
            status: TransferStatus {
                id,
                peer: link.name.clone(),
                peer_key: to_hex(&link.key),
                incoming,
                kind,
                label: label.to_string(),
                files,
                bytes,
                done: 0,
                state: TransferState::Active,
                note: None,
                can_reveal: false,
            },
            reveal: None,
            stop: stop.clone(),
        });
        while records.len() > MAX_RECORDS {
            match records.iter().position(|r| r.status.state != TransferState::Active) {
                Some(oldest) => drop(records.remove(oldest)),
                None => break,
            }
        }
        (id, stop)
    }

    fn with_record(&self, id: u32, change: impl FnOnce(&mut Record)) {
        if let Some(record) = self.records.lock().unwrap().iter_mut().find(|r| r.status.id == id) {
            change(record);
        }
    }

    fn close_record(&self, id: u32, state: TransferState, note: Option<String>) {
        self.with_record(id, |record| {
            if record.status.state == TransferState::Active {
                record.status.state = state;
                record.status.note = note;
            }
        });
    }

    fn link_alive(&self, link: &Arc<Link>) -> bool {
        self.links.lock().unwrap().get(&link.key).map_or(false, |l| Arc::ptr_eq(l, link))
    }

    // ----- sending -----

    /// The cursor is moving to (or back to) this computer: send it whatever
    /// was copied here, unless it already has it. Never blocks.
    fn carry(self: &Arc<Self>, link: &Arc<Link>) {
        let what = {
            let mut clip = self.clip.lock().unwrap();
            if matches!(clip.local, Carry::Nothing) || !clip.carried.insert(link.key) {
                return;
            }
            clip.local.clone()
        };
        let (node, link) = (self.clone(), link.clone());
        thread::spawn(move || {
            let settled = match what {
                Carry::Files(paths) => node.carry_files(&link, &paths, TransferKind::Clipboard),
                Carry::Image => node.carry_image(&link),
                Carry::Nothing => true,
            };
            // A dropped connection is worth another try on the next crossing.
            if !settled {
                node.clip.lock().unwrap().carried.remove(&link.key);
            }
        });
    }

    /// Returns false only if it should be tried again later.
    fn carry_files(&self, link: &Arc<Link>, paths: &[PathBuf], kind: TransferKind) -> bool {
        let limit = self.carry_limit.load(Ordering::Relaxed);
        let manifest = match transfer::manifest(paths, limit) {
            Ok(manifest) => manifest,
            Err(Refusal::Empty) => return true,
            Err(refusal) => {
                let (state, note) = match refusal {
                    Refusal::TooLarge(limit) => {
                        (TransferState::Skipped, format!("Larger than {}, so it was not carried over", human(limit)))
                    }
                    Refusal::TooMany => (TransferState::Skipped, "Too many files to carry over".to_string()),
                    Refusal::Unreadable(what) => (TransferState::Failed, format!("Could not read {what}")),
                    Refusal::Empty => unreachable!(),
                };
                let label = paths.first().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (record, _) = self.open_record(link, false, kind, &label, paths.len() as u32, 0);
                self.close_record(record, state, Some(note));
                return true;
            }
        };
        self.send_manifest(link, kind, &manifest).is_ok() || self.link_alive(link)
    }

    fn carry_image(&self, link: &Arc<Link>) -> bool {
        let Some(png) = self.platform.clipboard_image() else { return true };
        if png.len() as u64 > transfer::MAX_IMAGE || !picture::looks_like_png(&png) {
            return true;
        }
        let sent = (|| {
            let mut sending = self.begin_sending(link, TransferKind::Image, "Picture", 1, 0, png.len() as u64)?;
            sending.data(&png)?;
            sending.finish()
        })();
        sent.is_ok() || self.link_alive(link)
    }

    fn send_manifest(&self, link: &Arc<Link>, kind: TransferKind, manifest: &Manifest) -> Result<(), String> {
        let mut sending =
            self.begin_sending(link, kind, &manifest.label, manifest.files, manifest.items.len() as u32, manifest.bytes)?;
        let mut buf = vec![0u8; transfer::CHUNK];
        for item in &manifest.items {
            sending.entry(&item.rel, item.size, item.dir)?;
            if item.dir || item.size == 0 {
                continue;
            }
            let name = item.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Ok(mut file) = File::open(&item.source) else {
                return Err(sending.give_up(format!("Could not read {name}")));
            };
            let mut left = item.size;
            while left > 0 {
                let want = left.min(buf.len() as u64) as usize;
                match file.read(&mut buf[..want]) {
                    Ok(n) if n > 0 => {
                        sending.data(&buf[..n])?;
                        left -= n as u64;
                    }
                    _ => return Err(sending.give_up(format!("{name} changed while it was being sent"))),
                }
            }
        }
        sending.finish()
    }

    /// Send files or folders to a connected computer, where they are saved
    /// in its received-files folder. Returns at once; progress shows up in
    /// `transfers`.
    pub fn send_files(self: &Arc<Self>, key: &Key, paths: Vec<PathBuf>) -> Result<(), String> {
        let link = self.links.lock().unwrap().get(key).cloned().ok_or("that computer is not connected")?;
        let node = self.clone();
        thread::spawn(move || match transfer::manifest(&paths, u64::MAX) {
            Ok(manifest) => {
                let _ = node.send_manifest(&link, TransferKind::Sent, &manifest);
            }
            Err(refusal) => {
                let note = match refusal {
                    Refusal::Unreadable(what) => format!("Could not read {what}"),
                    Refusal::TooMany => "Too many files to send at once".to_string(),
                    _ => "Nothing to send".to_string(),
                };
                let label = paths.first().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (record, _) = node.open_record(&link, false, TransferKind::Sent, &label, paths.len() as u32, 0);
                node.close_record(record, TransferState::Failed, Some(note));
            }
        });
        Ok(())
    }

    /// Send one file whose contents arrive from `reader` (the settings
    /// window hands files over this way). Blocks until it has been sent.
    pub fn send_stream(&self, key: &Key, name: &str, size: u64, reader: &mut dyn Read) -> Result<(), String> {
        let link = self.links.lock().unwrap().get(key).cloned().ok_or("that computer is not connected")?;
        let name = transfer::safe_name(name);
        let mut sending = self.begin_sending(&link, TransferKind::Sent, &name, 1, 1, size)?;
        sending.entry(&name, size, false)?;
        let mut buf = vec![0u8; transfer::CHUNK];
        let mut left = size;
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            match reader.read(&mut buf[..want]) {
                Ok(n) if n > 0 => {
                    sending.data(&buf[..n])?;
                    left -= n as u64;
                }
                _ => return Err(sending.give_up("The file stopped arriving".into())),
            }
        }
        sending.finish()
    }

    fn begin_sending(
        &self,
        link: &Arc<Link>,
        kind: TransferKind,
        label: &str,
        files: u32,
        entries: u32,
        bytes: u64,
    ) -> Result<Sending<'_>, String> {
        let id = u64::from_le_bytes(random_bytes());
        let (record, stop) = self.open_record(link, false, kind, label, files, bytes);
        let out = Arc::new(Outgoing { progress: Mutex::new((0, false)), moved: Condvar::new(), stop });
        link.outgoing.lock().unwrap().insert(id, out.clone());
        let mut sending = Sending { node: self, link: link.clone(), id, record, out, sent: 0, done: false, note: None };
        sending.put(Msg::TransferBegin { id, kind, label: label.to_string(), files, entries, bytes })?;
        Ok(sending)
    }

    // ----- receiving -----

    fn incoming_begin(&self, link: &Arc<Link>, id: u64, kind: TransferKind, label: &str, files: u32, entries: u32, bytes: u64) {
        let mut incoming = link.incoming.lock().unwrap();
        if incoming.len() >= MAX_INCOMING || incoming.contains_key(&id) {
            self.send(link, Msg::TransferCancel { id });
            return;
        }
        let (record, stop) = self.open_record(link, true, kind, &clean_name(label), files, bytes);
        let refuse = |state: TransferState, note: String| {
            self.close_record(record, state, Some(note));
            self.send(link, Msg::TransferCancel { id });
        };
        let receiver = match kind {
            TransferKind::Image => {
                if bytes == 0 || bytes > transfer::MAX_IMAGE || entries != 0 {
                    return refuse(TransferState::Skipped, "That picture is too large".into());
                }
                None
            }
            TransferKind::Clipboard | TransferKind::Drag => {
                let limit = self.carry_limit.load(Ordering::Relaxed);
                if bytes > limit {
                    return refuse(TransferState::Skipped, format!("Larger than {}, so it was not accepted", human(limit)));
                }
                Some(Receiver::new(self.staging.join(format!("clip-{id:016x}")), entries, bytes))
            }
            TransferKind::Sent => Some(Receiver::new(self.received_dir.join(format!(".incoming-{id:016x}")), entries, bytes)),
        };
        let files = match receiver {
            Some(Ok(receiver)) => Some(receiver),
            Some(Err(why)) => return refuse(TransferState::Failed, why),
            None => None,
        };
        let ticket = if kind == TransferKind::Sent { 0 } else { self.clip.lock().unwrap().ticket() };
        incoming.insert(id, Incoming { record, kind, stop, files, image: Vec::new(), bytes, ticket });
    }

    /// Apply one message to a transfer under way. `step` returns the number
    /// of bytes received so far if that changed.
    fn incoming_step(&self, link: &Arc<Link>, id: u64, step: impl FnOnce(&mut Incoming) -> Result<Option<u64>, String>) {
        let mut all = link.incoming.lock().unwrap();
        // Unknown transfers are ones we already gave up on.
        let Some(incoming) = all.get_mut(&id) else { return };
        let by_user = incoming.stop.load(Ordering::Relaxed) == BY_USER;
        let result = if by_user { Err(String::new()) } else { step(incoming) };
        let record = incoming.record;
        match result {
            Ok(Some(received)) => {
                drop(all);
                self.with_record(record, |r| r.status.done = received);
                self.send(link, Msg::TransferAck { id, bytes: received });
            }
            Ok(None) => {}
            Err(why) => {
                // Dropping it removes whatever was written so far.
                drop(all.remove(&id));
                drop(all);
                if by_user {
                    self.close_record(record, TransferState::Cancelled, None);
                } else {
                    self.close_record(record, TransferState::Failed, Some(why));
                }
                self.send(link, Msg::TransferCancel { id });
            }
        }
    }

    fn incoming_end(&self, link: &Arc<Link>, id: u64) {
        let Some(incoming) = link.incoming.lock().unwrap().remove(&id) else { return };
        let record = incoming.record;
        match self.settle(link, incoming) {
            Ok((note, reveal)) => {
                self.with_record(record, |r| {
                    r.status.state = TransferState::Done;
                    r.status.done = r.status.bytes;
                    r.status.note = note;
                    r.status.can_reveal = reveal.is_some();
                    r.reveal = reveal;
                });
                self.send(link, Msg::TransferEnd { id });
            }
            Err(why) => {
                self.close_record(record, TransferState::Failed, Some(why));
                self.send(link, Msg::TransferCancel { id });
            }
        }
    }

    /// Everything has arrived: put it where it belongs. Returns a note for
    /// the user, if any, and the saved file that can be shown to them.
    fn settle(&self, link: &Arc<Link>, incoming: Incoming) -> Result<(Option<String>, Option<PathBuf>), String> {
        let newer = || Ok((Some("Not placed, because something newer was copied here".to_string()), None));
        match incoming.kind {
            TransferKind::Image => {
                if incoming.image.len() as u64 != incoming.bytes || !picture::looks_like_png(&incoming.image) {
                    return Err("The picture did not arrive intact".into());
                }
                if !self.clip.lock().unwrap().admit(incoming.ticket) {
                    return newer();
                }
                *self.last_clipboard.lock().unwrap() = None;
                // Decoding a large picture takes a moment; do not hold up
                // the keys and clicks arriving on this connection.
                let (platform, png) = (self.platform.clone(), incoming.image);
                thread::spawn(move || platform.set_clipboard_image(&png));
                Ok((None, None))
            }
            TransferKind::Drag => {
                let tops = incoming.files.ok_or("nothing arrived")?.finish()?;
                let hold = std::mem::replace(&mut *link.drag_hold.lock().unwrap(), Hold::None);
                if hold == Hold::Holding {
                    // The button is still down: the drag carries on here,
                    // now with the real files behind it.
                    self.drag_arrived(link, DragPayload::Files(tops));
                    return Ok((None, None));
                }
                // Let go too early to be dropped anywhere: keep the files
                // for pasting instead of losing them.
                if !self.clip.lock().unwrap().admit(incoming.ticket) {
                    return newer();
                }
                *self.last_clipboard.lock().unwrap() = None;
                self.platform.set_clipboard_files(&tops);
                Ok((Some("Let go before it arrived, so it is ready to paste".to_string()), None))
            }
            TransferKind::Clipboard => {
                let tops = incoming.files.ok_or("nothing arrived")?.finish()?;
                let root = tops[0].parent().map(Path::to_path_buf);
                if !self.clip.lock().unwrap().admit(incoming.ticket) {
                    if let Some(root) = root {
                        let _ = fs::remove_dir_all(root);
                    }
                    return newer();
                }
                *self.last_clipboard.lock().unwrap() = None;
                self.platform.set_clipboard_files(&tops);
                // The files offered before this are no longer on the clipboard.
                let previous = std::mem::replace(&mut *self.last_staged.lock().unwrap(), root);
                if let Some(previous) = previous {
                    let _ = fs::remove_dir_all(previous);
                }
                Ok((None, None))
            }
            TransferKind::Sent => {
                let tops = incoming.files.ok_or("nothing arrived")?.finish()?;
                let root = tops[0].parent().map(Path::to_path_buf);
                let mut first = None;
                let mut problem = None;
                for top in &tops {
                    let name = top.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let target = transfer::unique_path(&self.received_dir, &name);
                    match fs::rename(top, &target) {
                        Ok(()) => {
                            first.get_or_insert(target);
                        }
                        Err(e) => problem = Some(format!("Could not save {name}: {e}")),
                    }
                }
                if let Some(root) = root {
                    let _ = fs::remove_dir_all(root);
                }
                match problem {
                    Some(problem) => Err(problem),
                    None => Ok((None, first)),
                }
            }
        }
    }
}

/// One transfer being sent. Dropping it before `finish` cancels it on both
/// computers.
struct Sending<'a> {
    node: &'a Node,
    link: Arc<Link>,
    id: u64,
    record: u32,
    out: Arc<Outgoing>,
    sent: u64,
    done: bool,
    note: Option<String>,
}

impl Sending<'_> {
    fn put(&mut self, message: Msg) -> Result<(), String> {
        if self.out.stop.load(Ordering::Relaxed) != RUNNING {
            return Err("stopped".into());
        }
        // Written directly rather than through the shared queue, so that a
        // long transfer waits for the network instead of piling up in memory.
        self.link.stream.send(&message).map_err(|e| {
            self.link.stream.shutdown();
            self.note = Some("The connection was lost".into());
            e
        })
    }

    fn entry(&mut self, path: &str, size: u64, dir: bool) -> Result<(), String> {
        self.put(Msg::TransferEntry { id: self.id, path: path.to_string(), size, dir })
    }

    fn data(&mut self, bytes: &[u8]) -> Result<(), String> {
        for chunk in bytes.chunks(transfer::CHUNK) {
            self.wait_for_room(chunk.len() as u64)?;
            self.put(Msg::TransferData { id: self.id, data: chunk.to_vec() })?;
            self.sent += chunk.len() as u64;
        }
        let sent = self.sent;
        self.node.with_record(self.record, |r| r.status.done = sent);
        Ok(())
    }

    /// Wait until the receiver has confirmed enough that `next` more bytes
    /// stay inside the window.
    fn wait_for_room(&mut self, next: u64) -> Result<(), String> {
        let out = self.out.clone();
        let mut progress = out.progress.lock().unwrap();
        let (mut seen, mut since) = (progress.0, Instant::now());
        loop {
            if out.stop.load(Ordering::Relaxed) != RUNNING {
                return Err("stopped".into());
            }
            if self.sent + next <= progress.0.saturating_add(transfer::WINDOW) {
                return Ok(());
            }
            if progress.0 != seen {
                (seen, since) = (progress.0, Instant::now());
            } else if since.elapsed() > STALL_TIMEOUT {
                self.note = Some("The other computer stopped responding".into());
                return Err("stalled".into());
            }
            progress = out.moved.wait_timeout(progress, Duration::from_millis(200)).unwrap().0;
        }
    }

    /// Everything has been sent; wait for the other computer to say it is
    /// all in place.
    fn finish(mut self) -> Result<(), String> {
        self.put(Msg::TransferEnd { id: self.id })?;
        let out = self.out.clone();
        let deadline = Instant::now() + FINISH_TIMEOUT;
        let mut progress = out.progress.lock().unwrap();
        while !progress.1 {
            if out.stop.load(Ordering::Relaxed) != RUNNING {
                return Err("stopped".into());
            }
            if Instant::now() > deadline {
                self.note = Some("The other computer did not confirm".into());
                return Err("unconfirmed".into());
            }
            progress = out.moved.wait_timeout(progress, Duration::from_millis(200)).unwrap().0;
        }
        drop(progress);
        self.done = true;
        self.node.with_record(self.record, |r| {
            r.status.done = r.status.bytes;
            r.status.state = TransferState::Done;
        });
        Ok(())
    }

    /// Stop because of a problem on this side; returns the reason.
    fn give_up(mut self, note: String) -> String {
        self.note = Some(note.clone());
        note
    }
}

impl Drop for Sending<'_> {
    fn drop(&mut self) {
        self.link.outgoing.lock().unwrap().remove(&self.id);
        if self.done {
            return;
        }
        let why = self.out.stop.load(Ordering::Relaxed);
        if why == RUNNING || why == BY_USER {
            let _ = self.link.stream.send(&Msg::TransferCancel { id: self.id });
        }
        let (state, note) = match why {
            BY_USER => (TransferState::Cancelled, None),
            BY_PEER => (TransferState::Failed, Some("The other computer did not accept it".to_string())),
            LINK_LOST => (TransferState::Failed, Some("The connection was lost".to_string())),
            _ => (TransferState::Failed, self.note.take().or_else(|| Some("Could not be sent".to_string()))),
        };
        self.node.close_record(self.record, state, note);
    }
}

/// The only thread that writes to the network, so hooks never block on it.
fn send_loop(outbox: mpsc::Receiver<Out>, udp: UdpSocket) {
    for out in outbox {
        match out {
            Out::Tcp(link, message) => {
                if link.stream.send(&message).is_err() {
                    link.stream.shutdown();
                }
            }
            Out::Mouse(link, x, y) => {
                if link.udp_ok.load(Ordering::Relaxed) {
                    let _ = udp.send_to(&link.sealer.seal(&Dgram::MouseAbs { x, y }), link.udp_addr);
                } else if link.stream.send(&Msg::MouseAbs { x, y }).is_err() {
                    link.stream.shutdown();
                }
            }
            Out::Probe(link) => {
                let _ = udp.send_to(&link.sealer.seal(&Dgram::Probe), link.udp_addr);
            }
            // Sealed here, on the one sending thread, so datagrams leave in
            // the order they were numbered.
            Out::Audio(link, sound) => {
                let _ = udp.send_to(&link.sealer.seal(&sound), link.udp_addr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> Clip {
        Clip { local: Carry::Nothing, carried: HashSet::new(), stamp: 0, latest: 0 }
    }

    #[test]
    fn newest_copy_wins_whatever_order_transfers_finish_in() {
        // Files were copied first, a picture second; the picture arrives first.
        let mut c = clip();
        let (files, picture) = (c.ticket(), c.ticket());
        assert!(c.admit(picture));
        assert!(!c.admit(files), "the older copy must not replace the newer one");

        // The same two arriving in the order they were copied both land.
        let mut c = clip();
        let (first, second) = (c.ticket(), c.ticket());
        assert!(c.admit(first));
        assert!(c.admit(second));

        // Something copied here while a transfer was under way stays.
        let mut c = clip();
        let slow = c.ticket();
        c.changed(Carry::Image);
        assert!(!c.admit(slow));
        assert!(matches!(c.local, Carry::Image));
    }

    #[test]
    fn a_new_copy_is_carried_again() {
        let mut c = clip();
        c.changed(Carry::Image);
        assert!(c.carried.insert([1u8; 32]));
        assert!(!c.carried.insert([1u8; 32]), "the same copy is not sent twice");
        c.changed(Carry::Image);
        assert!(c.carried.insert([1u8; 32]));
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(human(1 << 30), "1 GB");
        assert_eq!(human(1536 * 1024), "1.5 MB");
        assert_eq!(human(64 * 1024 * 1024), "64 MB");
        assert_eq!(human(u64::MAX), "16777216 TB");
    }
}
