//! Sound on Linux, through the desktop's sound server.
//!
//! Both PulseAudio and PipeWire (which nearly every desktop uses, and which
//! answers to the same tools) come with three small programs: `parec`
//! records, `pacat` plays and `pactl` changes settings. This file runs
//! those instead of linking a sound library, which keeps the program free
//! of C libraries to build against.
//!
//! - **Sending our sound away**: record the "monitor" of the speakers
//!   (everything this computer plays) and mute the speakers meanwhile. The
//!   monitor is tapped before the mute, so recording still hears it all.
//! - **Playing others' sound**: mix what arrives and feed it to `pacat`.

use crate::audio::Player;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Frames per second used for both recording and playing.
const RATE: u32 = 48_000;

fn log(text: &str) {
    eprintln!("[screen-portals] sound: {text}");
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A note left on disk while this program has the speakers muted, holding
/// the name of the muted output. If the program is killed or the computer
/// shuts down meanwhile, the note is how the next start knows to give the
/// speakers back.
fn marker() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
    dir.join(format!("screen-portals-{}-muted", unsafe { libc::getuid() }))
}

/// Undo a mute left behind by a run that ended without tidying up. Called
/// once when the program starts.
pub fn recover() {
    let Ok(sink) = std::fs::read_to_string(marker()) else { return };
    let sink = sink.trim();
    if sink.is_empty() || pactl(&["set-sink-mute", sink, "0"]).is_some() {
        let _ = std::fs::remove_file(marker());
        log("the speakers were still muted from last time; they are back on");
    }
}

/// Mute the speakers in use. Returns their name if the mute is ours to undo.
fn silence_speakers() -> Option<String> {
    let sink = pactl(&["get-default-sink"]).filter(|name| !name.is_empty())?;
    let already = pactl(&["get-sink-mute", &sink]).is_some_and(|state| state.to_ascii_lowercase().ends_with("yes"));
    // Muted already with our note present means an earlier run of ours did
    // it, so it is still ours to undo.
    let ours = if already { marker().exists() } else { pactl(&["set-sink-mute", &sink, "1"]).is_some() };
    if !ours {
        return None;
    }
    let _ = std::fs::write(marker(), &sink);
    Some(sink)
}

fn restore_speakers(sink: &str) {
    if pactl(&["set-sink-mute", sink, "0"]).is_some() {
        let _ = std::fs::remove_file(marker());
    }
}

fn recorder() -> std::io::Result<Child> {
    Command::new("parec")
        .args(["--device=@DEFAULT_MONITOR@", "--format=s16le", "--channels=2", "--raw", "--latency-msec=20"])
        .arg(format!("--rate={RATE}"))
        .arg("--client-name=Screen Portals")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// Read recorded sound from `parec` and hand it on in small pieces until
/// told to stop or the recorder ends.
fn pump(mut from: impl Read, stop: &AtomicBool, mut sink: impl FnMut(&[i16])) {
    let mut bytes = [0u8; 4096];
    let mut held = 0; // bytes of an unfinished frame kept from last time
    while !stop.load(Ordering::Relaxed) {
        let got = match from.read(&mut bytes[held..]) {
            Ok(0) | Err(_) => break,
            Ok(got) => got + held,
        };
        let whole = got - got % 4; // one frame is two 16-bit samples
        let samples: Vec<i16> = bytes[..whole].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        sink(&samples);
        bytes.copy_within(whole..got, 0);
        held = got - whole;
    }
}

struct Capture {
    recorder: Child,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// The output we muted, if we did.
    muted: Option<String>,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);

/// Start or stop sending this computer's sound away. Returns once the
/// change has been made.
pub fn capture(on: bool) {
    let mut capture = CAPTURE.lock().unwrap();
    if on == capture.is_some() {
        return;
    }
    if let Some(mut running) = capture.take() {
        running.stop.store(true, Ordering::Relaxed);
        let _ = running.recorder.kill();
        let _ = running.recorder.wait();
        if let Some(reader) = running.reader.take() {
            let _ = reader.join();
        }
        if let Some(sink) = running.muted {
            restore_speakers(&sink);
        }
        return;
    }
    let mut recorder = match recorder() {
        Ok(recorder) => recorder,
        Err(e) => {
            // Nothing was muted, so this computer simply keeps its sound.
            log(&format!("cannot record what this computer plays (is `parec` installed?): {e}"));
            return;
        }
    };
    let Some(recorded) = recorder.stdout.take() else { return };
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            pump(recorded, &stop, |samples| {
                if let Some(node) = super::linux::node() {
                    node.on_audio_captured(RATE, samples);
                }
            })
        })
    };
    *capture = Some(Capture { recorder, stop, reader: Some(reader), muted: silence_speakers() });
}

static PLAYER: OnceLock<Mutex<Player>> = OnceLock::new();
static PLAYING: AtomicBool = AtomicBool::new(false);

fn player() -> &'static Mutex<Player> {
    PLAYER.get_or_init(|| Mutex::new(Player::new(RATE)))
}

/// Queue sound from another computer for playing.
pub fn play(source: u64, seq: u32, rate: u32, stereo: &[i16]) {
    player().lock().unwrap().push(source, seq, rate, stereo);
    if !PLAYING.swap(true, Ordering::Relaxed) {
        std::thread::spawn(|| loop {
            play_until_quiet();
            PLAYING.store(false, Ordering::Relaxed);
            // Sound may have arrived in the instant before that line.
            if player().lock().unwrap().is_idle() || PLAYING.swap(true, Ordering::Relaxed) {
                break;
            }
        });
    }
}

fn speaker() -> std::io::Result<Child> {
    Command::new("pacat")
        .args(["--playback", "--format=s16le", "--channels=2", "--raw", "--latency-msec=30"])
        .arg(format!("--rate={RATE}"))
        .args(["--client-name=Screen Portals", "--stream-name=Sound from your other computers"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Feed mixed sound to `pacat` at a steady pace until no computer has sent
/// anything for a while.
fn play_until_quiet() {
    let mut speaker = match speaker() {
        Ok(speaker) => speaker,
        Err(e) => {
            log(&format!("cannot play sound from other computers (is `pacat` installed?): {e}"));
            // Do not start a new player for every packet that arrives.
            std::thread::sleep(Duration::from_secs(5));
            return;
        }
    };
    if let Some(mut pipe) = speaker.stdin.take() {
        // A pipe normally holds a third of a second of sound, which would
        // be heard as delay. Shrink it to about 20 ms.
        unsafe {
            libc::fcntl(pipe.as_raw_fd(), libc::F_SETPIPE_SZ, 4096);
        }
        const TICK: Duration = Duration::from_millis(10);
        let frames = (RATE / 100) as usize;
        let mut mixed = vec![0f32; frames * 2];
        let mut bytes = Vec::with_capacity(frames * 4);
        let mut due = Instant::now();
        loop {
            let idle = {
                let mut player = player().lock().unwrap();
                player.pull(&mut mixed, 2);
                player.is_idle()
            };
            if idle {
                break;
            }
            bytes.clear();
            for sample in &mixed {
                bytes.extend_from_slice(&((sample * 32767.0) as i16).to_le_bytes());
            }
            if pipe.write_all(&bytes).is_err() {
                break;
            }
            due += TICK;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            } else if now - due > Duration::from_millis(100) {
                due = now; // fell behind (the computer was busy): start afresh
            }
        }
    }
    let _ = speaker.kill();
    let _ = speaker.wait();
}

/// Really record this computer's own sound for a moment: mute the
/// speakers, play a quiet tone, and check the recording heard it even
/// though the speakers were silent. Everything is put back afterwards.
pub fn self_test() -> Result<String, String> {
    let server = pactl(&["info"]).ok_or("no sound server answered (`pactl info` failed); PulseAudio or PipeWire with its tools is needed")?;
    let name = server.lines().find_map(|line| line.strip_prefix("Server Name: ")).unwrap_or("unknown").to_string();
    let mut recorder = recorder().map_err(|e| format!("cannot start `parec`: {e}"))?;
    let recorded = recorder.stdout.take().ok_or("cannot read from `parec`")?;
    let muted = silence_speakers();
    let stop = Arc::new(AtomicBool::new(false));
    let loudest = Arc::new(Mutex::new(0i32));
    let reader = {
        let (stop, loudest) = (stop.clone(), loudest.clone());
        std::thread::spawn(move || {
            pump(recorded, &stop, |samples| {
                let peak = samples.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
                let mut loudest = loudest.lock().unwrap();
                *loudest = (*loudest).max(peak);
            })
        })
    };
    // A second of a 440 Hz tone at a tenth of full volume.
    let tone: Vec<u8> = (0..RATE)
        .flat_map(|n| {
            let level = ((n as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 3276.0) as i16;
            [level.to_le_bytes(), level.to_le_bytes()].concat()
        })
        .collect();
    let played = speaker().and_then(|mut speaker| {
        let written = speaker.stdin.take().map(|mut pipe| pipe.write_all(&tone));
        std::thread::sleep(Duration::from_millis(1500));
        let _ = speaker.kill();
        let _ = speaker.wait();
        written.unwrap_or(Ok(()))
    });
    stop.store(true, Ordering::Relaxed);
    let _ = recorder.kill();
    let _ = recorder.wait();
    let _ = reader.join();
    if let Some(sink) = &muted {
        restore_speakers(sink);
    }
    played.map_err(|e| format!("cannot play through `pacat`: {e}"))?;
    let loudest = *loudest.lock().unwrap();
    if loudest < 500 {
        return Err(format!(
            "the test tone was not heard in the recording (loudest sample {loudest}) on {name}; this computer's sound cannot be sent to another one"
        ));
    }
    let muting = if muted.is_some() { "speakers muted meanwhile" } else { "speakers were muted already" };
    Ok(format!("{name}; heard the test tone, {muting}"))
}
