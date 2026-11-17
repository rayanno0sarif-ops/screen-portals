//! Sound on macOS: playing what other computers send, through the Mac's
//! current output.
//!
//! The other direction (sending the Mac's own sound away) would need a
//! way to record everything the Mac plays, which macOS does not offer to
//! an ordinary program without extra software, so it is not done yet; see
//! `MacPlatform::can_send_audio`.

use crate::audio::Player;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

static PLAYER: OnceLock<Mutex<Player>> = OnceLock::new();
static WAKE: OnceLock<Mutex<mpsc::Sender<()>>> = OnceLock::new();
static PLAYING: AtomicBool = AtomicBool::new(false);
static BROKEN: AtomicBool = AtomicBool::new(false);

fn log(text: &str) {
    eprintln!("[screen-portals] sound: {text}");
}

fn player() -> &'static Mutex<Player> {
    PLAYER.get_or_init(|| Mutex::new(Player::new(48_000)))
}

/// Queue sound from another computer for playing.
pub fn play(source: u64, seq: u32, rate: u32, stereo: &[i16]) {
    player().lock().unwrap().push(source, seq, rate, stereo);
    if !PLAYING.swap(true, Ordering::Relaxed) {
        let wake = WAKE.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || run(rx));
            Mutex::new(tx)
        });
        let _ = wake.lock().unwrap().send(());
    }
}

fn open() -> Result<cpal::Stream, String> {
    let err = |e: &dyn std::fmt::Display| e.to_string();
    let device = cpal::default_host().default_output_device().ok_or("this Mac has no sound output")?;
    let supported = device.default_output_config().map_err(|e| err(&e))?;
    let config: cpal::StreamConfig = supported.clone().into();
    let channels = config.channels as usize;
    player().lock().unwrap().set_out_rate(config.sample_rate);
    let failed = |_: cpal::Error| BROKEN.store(true, Ordering::Relaxed);
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
        other => return Err(format!("the sound output uses a format that is not supported ({other})")),
    }
    .map_err(|e| err(&e))?;
    stream.play().map_err(|e| err(&e))?;
    Ok(stream)
}

/// Keeps the output open while sound arrives, and lets it go once no
/// computer has sent anything for a while.
fn run(wake: mpsc::Receiver<()>) {
    let mut stream: Option<cpal::Stream> = None;
    loop {
        match wake.recv_timeout(Duration::from_secs(1)) {
            Ok(()) => {
                if stream.is_none() {
                    match open() {
                        Ok(opened) => stream = Some(opened),
                        Err(e) => log(&format!("cannot play sound from other computers: {e}")),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if BROKEN.swap(false, Ordering::Relaxed) || player().lock().unwrap().is_idle() {
                    stream = None;
                }
                if stream.is_none() {
                    PLAYING.store(false, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// The name of the output sound from other computers would play through.
pub fn output_name() -> Option<String> {
    let device = cpal::default_host().default_output_device()?;
    device.description().ok().map(|d| d.to_string())
}
