//! Windows sound: capturing everything this computer plays ("loopback"),
//! silencing its speakers meanwhile, and playing sound from other computers.
//!
//! One thread owns the devices. Everything else talks to it through a
//! channel, so a slow or missing sound device can never hold up input.

use crate::audio::{self, Player};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

enum Cmd {
    /// Start or stop capturing; the sender is told when it has happened.
    Capture(bool, mpsc::Sender<()>),
    /// Sound has arrived to be played.
    Wake,
}

static COMMANDS: OnceLock<Mutex<mpsc::Sender<Cmd>>> = OnceLock::new();
static PLAYER: OnceLock<Mutex<Player>> = OnceLock::new();
/// The output device is open (or being opened) for other computers' sound.
static PLAYING: AtomicBool = AtomicBool::new(false);
static CAPTURE_BROKEN: AtomicBool = AtomicBool::new(false);
static PLAY_BROKEN: AtomicBool = AtomicBool::new(false);
/// During the self-test captured sound goes here instead of to the engine.
static TAP: Mutex<Option<mpsc::Sender<(u32, Vec<i16>)>>> = Mutex::new(None);

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn log(text: &str) {
    eprintln!("[screen-portals] {text}");
}

fn player() -> &'static Mutex<Player> {
    PLAYER.get_or_init(|| Mutex::new(Player::new(48_000)))
}

fn commands() -> mpsc::Sender<Cmd> {
    COMMANDS
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || run(rx));
            Mutex::new(tx)
        })
        .lock()
        .unwrap()
        .clone()
}

/// Start or stop sending this computer's sound away. Returns once the
/// change has been made (or after a few seconds if the device is stuck).
pub fn capture(on: bool) {
    let (done, wait) = mpsc::channel();
    if commands().send(Cmd::Capture(on, done)).is_ok() {
        let _ = wait.recv_timeout(Duration::from_secs(3));
    }
}

/// Queue sound from another computer for playing.
pub fn play(source: u64, seq: u32, rate: u32, stereo: &[i16]) {
    player().lock().unwrap().push(source, seq, rate, stereo);
    if !PLAYING.swap(true, Ordering::Relaxed) {
        let _ = commands().send(Cmd::Wake);
    }
}

fn deliver(rate: u32, stereo: &[i16]) {
    if let Some(tap) = TAP.lock().unwrap().as_ref() {
        let _ = tap.send((rate, stereo.to_vec()));
        return;
    }
    if let Some(node) = super::windows::node() {
        node.on_audio_captured(rate, stereo);
    }
}

/// Glitches are reported as errors too; only real failures need a rebuild.
fn serious(error: &cpal::Error) -> bool {
    !matches!(error.kind(), cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied)
}

/// A running capture. Dropping it gives the speakers back.
struct Capturing {
    _stream: cpal::Stream,
    volume: Option<IAudioEndpointVolume>,
    muted_by_us: bool,
}

impl Drop for Capturing {
    fn drop(&mut self) {
        // Only undo what we did: speakers the user had muted stay muted.
        if let (true, Some(volume)) = (self.muted_by_us, &self.volume) {
            unsafe {
                if volume.SetMute(false, std::ptr::null()).is_ok() {
                    let _ = std::fs::remove_file(marker());
                }
            }
        }
    }
}

/// A note on disk saying "the speakers are muted because of us". Windows
/// remembers a mute across restarts, so if this program is killed or the
/// computer shuts down while its sound is being sent away, the note is how
/// the next start knows to give the speakers back.
fn marker() -> std::path::PathBuf {
    std::env::temp_dir().join("ScreenPortals").join("speakers-muted-by-screen-portals")
}

unsafe fn default_speakers() -> windows::core::Result<IAudioEndpointVolume> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)
}

/// Whether the default speakers are muted right now, if that can be told.
pub fn speakers_muted() -> Option<bool> {
    unsafe { default_speakers().ok()?.GetMute().ok().map(|m| m.as_bool()) }
}

/// Undo a mute left behind by a run that ended without tidying up. Called
/// once when the program starts.
pub fn recover() {
    if !marker().exists() {
        return;
    }
    unsafe {
        if let Ok(volume) = default_speakers() {
            if volume.SetMute(false, std::ptr::null()).is_ok() {
                let _ = std::fs::remove_file(marker());
                log("the speakers were still muted from last time; they are back on");
            }
        }
    }
}

/// Mute the default speakers. Capturing still hears everything, because
/// Windows taps the sound before the mute is applied.
unsafe fn silence_speakers() -> (Option<IAudioEndpointVolume>, bool) {
    match default_speakers() {
        Ok(volume) => {
            let already = volume.GetMute().map(|m| m.as_bool()).unwrap_or(true);
            // Muted already with our note present means an earlier run of
            // ours did it, so it is still ours to undo.
            let ours = if already { marker().exists() } else { volume.SetMute(true, std::ptr::null()).is_ok() };
            if ours {
                let _ = std::fs::create_dir_all(marker().parent().unwrap_or(std::path::Path::new(".")));
                let _ = std::fs::write(marker(), "");
            }
            (Some(volume), ours)
        }
        Err(_) => (None, false),
    }
}

fn start_capture() -> Result<Capturing, String> {
    let device = cpal::default_host().default_output_device().ok_or("this computer has no sound output")?;
    let supported = device.default_output_config().map_err(err)?;
    let config: cpal::StreamConfig = supported.clone().into();
    let (channels, rate) = (config.channels as usize, config.sample_rate);
    let failed = |error: cpal::Error| {
        if serious(&error) {
            CAPTURE_BROKEN.store(true, Ordering::Relaxed);
        }
    };
    // Asking an output device for input is how Windows offers "what is
    // playing right now".
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| deliver(rate, &audio::stereo_from_f32(data, channels)),
            failed,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| deliver(rate, &audio::stereo_from_i16(data, channels)),
            failed,
            None,
        ),
        other => return Err(format!("the sound device uses a format that is not supported ({other})")),
    }
    .map_err(err)?;
    stream.play().map_err(err)?;
    let (volume, muted_by_us) = unsafe { silence_speakers() };
    Ok(Capturing { _stream: stream, volume, muted_by_us })
}

fn open_playback() -> Result<cpal::Stream, String> {
    let device = cpal::default_host().default_output_device().ok_or("this computer has no sound output")?;
    let supported = device.default_output_config().map_err(err)?;
    let config: cpal::StreamConfig = supported.clone().into();
    let channels = config.channels as usize;
    player().lock().unwrap().set_out_rate(config.sample_rate);
    let failed = |error: cpal::Error| {
        if serious(&error) {
            PLAY_BROKEN.store(true, Ordering::Relaxed);
        }
    };
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| player().lock().unwrap().pull(data, channels),
            failed,
            None,
        ),
        cpal::SampleFormat::I16 => {
            let mut scratch: Vec<f32> = Vec::new();
            device.build_output_stream(
                config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    scratch.resize(data.len(), 0.0);
                    player().lock().unwrap().pull(&mut scratch, channels);
                    for (out, sample) in data.iter_mut().zip(&scratch) {
                        *out = (sample * 32767.0) as i16;
                    }
                },
                failed,
                None,
            )
        }
        other => return Err(format!("the sound device uses a format that is not supported ({other})")),
    }
    .map_err(err)?;
    stream.play().map_err(err)?;
    Ok(stream)
}

fn run(commands: mpsc::Receiver<Cmd>) {
    let mut want_capture = false;
    let mut capturing: Option<Capturing> = None;
    let mut playback: Option<cpal::Stream> = None;
    loop {
        match commands.recv_timeout(Duration::from_secs(1)) {
            Ok(Cmd::Capture(on, done)) => {
                want_capture = on;
                capturing = None; // gives the speakers back
                if on {
                    capturing = start_capture().map_err(|e| log(&format!("cannot capture this computer's sound: {e}"))).ok();
                }
                let _ = done.send(());
            }
            Ok(Cmd::Wake) => {
                if playback.is_none() {
                    match open_playback() {
                        Ok(stream) => playback = Some(stream),
                        Err(e) => log(&format!("cannot play sound from other computers: {e}")),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Housekeeping once a second: recover from unplugged or
                // changed devices, and let go of the output when it is idle
                // so it does not keep the computer awake.
                if CAPTURE_BROKEN.swap(false, Ordering::Relaxed) {
                    capturing = None;
                }
                if want_capture && capturing.is_none() {
                    capturing = start_capture().ok();
                }
                if PLAY_BROKEN.swap(false, Ordering::Relaxed) || player().lock().unwrap().is_idle() {
                    playback = None;
                }
                if playback.is_none() {
                    PLAYING.store(false, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Check the whole sound path on this computer without anyone hearing it:
/// the speakers are silenced, a tone is played the way another computer's
/// sound would be, and capturing must hear that tone. Takes about a second.
pub fn self_test() -> Result<String, String> {
    let muted_before = speakers_muted();
    let (tap, heard) = mpsc::channel();
    *TAP.lock().unwrap() = Some(tap);
    capture(true);
    let started = Instant::now();
    let mut seq = 0u32;
    // 0.7 s of a 440 Hz tone, handed over at the pace a network would.
    while started.elapsed() < Duration::from_millis(700) {
        let due = (started.elapsed().as_secs_f64() * 48_000.0 / audio::PACKET_FRAMES as f64) as u32;
        while seq < due {
            let block: Vec<i16> = (0..audio::PACKET_FRAMES)
                .flat_map(|i| {
                    let n = (seq as usize * audio::PACKET_FRAMES + i) as f32;
                    let v = ((n * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 1600.0) as i16;
                    [v, v]
                })
                .collect();
            seq += 1;
            play(u64::MAX, seq, 48_000, &block);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::sleep(Duration::from_millis(250));
    capture(false);
    *TAP.lock().unwrap() = None;

    let (mut rate, mut frames, mut energy) = (0u32, 0u64, 0f64);
    for (block_rate, block) in heard.try_iter() {
        rate = block_rate;
        frames += block.len() as u64 / 2;
        energy += block.iter().map(|s| (*s as f64 / 32768.0).powi(2)).sum::<f64>();
    }
    if frames == 0 {
        return Err("nothing could be captured from this computer's sound output".into());
    }
    let level = (energy / (frames as f64 * 2.0)).sqrt();
    if level < 0.005 {
        return Err(format!("the test tone was not heard (level {level:.4}); sound from other computers may not play here"));
    }
    let muted_after = speakers_muted();
    if muted_after != muted_before {
        return Err("the speakers were not put back the way they were".into());
    }
    let state = match muted_after {
        Some(true) => "speakers muted before and after",
        Some(false) => "speakers on before and after",
        None => "speaker state unknown",
    };
    Ok(format!("{rate} Hz, test tone heard at level {level:.3}, {state}"))
}
