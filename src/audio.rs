//! Sound travelling from one computer's programs to another computer's
//! speakers. Everything here is plain arithmetic with no devices involved.
//!
//! On the wire sound is always 16-bit stereo at the sending computer's own
//! rate. The playing side converts to its rate and smooths out the network:
//! packets arrive in bursts and the two sound cards never run at exactly the
//! same speed, so a small reserve is kept and gently steered towards a
//! target size.

use std::collections::{HashMap, VecDeque};

/// Stereo frames per packet: about 6 ms, and small enough for one datagram.
pub const PACKET_FRAMES: usize = 256;
/// Reserve kept before playing starts, and the level it is steered towards.
const TARGET_MS: u64 = 60;
/// Above this the oldest sound is thrown away to catch up.
const HIGH_MS: u64 = 200;
/// A sender that has been quiet this long is forgotten.
const FORGET_MS: u64 = 3000;
const MAX_SOURCES: usize = 8;

/// Turn captured samples with any number of channels into 16-bit stereo.
pub fn stereo_from_f32(samples: &[f32], channels: usize) -> Vec<i16> {
    let convert = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
    stereo(samples, channels, convert)
}

/// The same for devices that already hand out 16-bit samples.
pub fn stereo_from_i16(samples: &[i16], channels: usize) -> Vec<i16> {
    stereo(samples, channels, |v| v)
}

fn stereo<T: Copy>(samples: &[T], channels: usize, convert: impl Fn(T) -> i16) -> Vec<i16> {
    if channels == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(samples.len() / channels * 2);
    for frame in samples.chunks_exact(channels) {
        let left = convert(frame[0]);
        // Mono is played on both sides; surround keeps its front pair.
        let right = if channels > 1 { convert(frame[1]) } else { left };
        out.extend_from_slice(&[left, right]);
    }
    out
}

/// Samples as the bytes that go on the wire (little-endian).
pub fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// The reverse of `pcm_bytes`. A stray odd byte at the end is ignored.
pub fn pcm_from_bytes(bytes: &[u8]) -> Vec<i16> {
    bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
}

/// Decides whether captured sound is worth sending. Nothing is sent while
/// the computer is silent, apart from a short tail so sounds end cleanly.
#[derive(Default)]
pub struct SilenceGate {
    quiet_frames: u64,
}

impl SilenceGate {
    /// Returns true if this block of sound should be sent.
    pub fn pass(&mut self, stereo: &[i16], rate: u32) -> bool {
        if stereo.iter().any(|s| *s != 0) {
            self.quiet_frames = 0;
            return true;
        }
        self.quiet_frames += stereo.len() as u64 / 2;
        self.quiet_frames <= rate as u64 / 4
    }
}

struct Source {
    rate: u32,
    frames: VecDeque<[f32; 2]>,
    /// How far between the first and second waiting frame playback is.
    pos: f64,
    playing: bool,
    last_seq: u32,
    /// Output frames produced since anything arrived.
    quiet_for: u64,
}

/// Mixes the sound arriving from other computers into what this computer's
/// output device asks for.
pub struct Player {
    out_rate: u32,
    sources: HashMap<u64, Source>,
}

impl Player {
    /// `out_rate` is the rate of this computer's output device, in frames per second.
    pub fn new(out_rate: u32) -> Player {
        Player { out_rate: out_rate.max(1), sources: HashMap::new() }
    }

    /// Called when the output device (and so its rate) changes.
    pub fn set_out_rate(&mut self, out_rate: u32) {
        self.out_rate = out_rate.max(1);
    }

    /// True when no computer has sent anything for a while, so the output
    /// device can be released.
    pub fn is_idle(&self) -> bool {
        self.sources.is_empty()
    }

    /// Sound has arrived. `source` tells senders apart; `seq` counts their
    /// packets so late duplicates can be dropped.
    pub fn push(&mut self, source: u64, seq: u32, rate: u32, stereo: &[i16]) {
        if !(8_000..=192_000).contains(&rate) || stereo.len() < 2 {
            return;
        }
        if !self.sources.contains_key(&source) && self.sources.len() >= MAX_SOURCES {
            return;
        }
        let entry = self.sources.entry(source).or_insert_with(|| Source {
            rate,
            frames: VecDeque::new(),
            pos: 0.0,
            playing: false,
            last_seq: seq.wrapping_sub(1),
            quiet_for: 0,
        });
        if entry.rate != rate {
            entry.rate = rate;
            entry.frames.clear();
            entry.pos = 0.0;
            entry.playing = false;
        }
        // Old or repeated packets are dropped; a big jump back is a restart.
        let behind = entry.last_seq.wrapping_sub(seq);
        if behind < 1000 {
            return;
        }
        entry.last_seq = seq;
        entry.quiet_for = 0;
        for frame in stereo.chunks_exact(2) {
            entry.frames.push_back([frame[0] as f32 / 32768.0, frame[1] as f32 / 32768.0]);
        }
        let (target, high) = (rate as u64 * TARGET_MS / 1000, rate as u64 * HIGH_MS / 1000);
        if entry.frames.len() as u64 > high {
            let excess = entry.frames.len() - target as usize;
            entry.frames.drain(..excess);
            entry.pos = 0.0;
        }
    }

    /// Fill `out` (interleaved, `channels` per frame) with the next sound.
    pub fn pull(&mut self, out: &mut [f32], channels: usize) {
        out.fill(0.0);
        if channels == 0 {
            return;
        }
        let out_frames = out.len() / channels;
        let out_rate = self.out_rate;
        for source in self.sources.values_mut() {
            source.quiet_for += out_frames as u64;
            let target = (source.rate as u64 * TARGET_MS / 1000) as f64;
            if !source.playing {
                if (source.frames.len() as f64) < target {
                    continue;
                }
                source.playing = true;
            }
            // Play a touch faster when the reserve is full and slower when
            // it is low; half a percent is far below what an ear notices.
            let fullness = (source.frames.len() as f64 - target) / target;
            let step = source.rate as f64 / out_rate as f64 * (1.0 + (fullness * 0.01).clamp(-0.005, 0.005));
            for frame in out.chunks_exact_mut(channels) {
                let at = source.pos as usize;
                if at + 1 >= source.frames.len() {
                    // Ran dry: go quiet and build the reserve up again.
                    source.frames.clear();
                    source.pos = 0.0;
                    source.playing = false;
                    break;
                }
                let (a, b) = (source.frames[at], source.frames[at + 1]);
                let t = (source.pos - at as f64) as f32;
                let (left, right) = (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t);
                if channels == 1 {
                    frame[0] += (left + right) * 0.5;
                } else {
                    frame[0] += left;
                    frame[1] += right;
                }
                source.pos += step;
            }
            let used = (source.pos as usize).min(source.frames.len());
            source.frames.drain(..used);
            source.pos -= used as f64;
        }
        let forget = out_rate as u64 * FORGET_MS / 1000;
        self.sources.retain(|_, source| source.quiet_for < forget);
        for sample in out.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(frames: usize, start: i16) -> Vec<i16> {
        (0..frames).flat_map(|i| [start + i as i16, -(start + i as i16)]).collect()
    }

    #[test]
    fn any_channel_layout_becomes_stereo() {
        assert_eq!(stereo_from_f32(&[0.5, -0.5], 1), vec![16384, 16384, -16384, -16384]);
        assert_eq!(stereo_from_f32(&[1.0, -1.0, 2.0, -2.0], 2), vec![32767, -32767, 32767, -32767]);
        // Six channels: only the front pair is kept.
        assert_eq!(stereo_from_i16(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], 6), vec![1, 2, 7, 8]);
        assert!(stereo_from_i16(&[1, 2], 0).is_empty());
        let samples = [0i16, -1, i16::MAX, i16::MIN];
        assert_eq!(pcm_from_bytes(&pcm_bytes(&samples)), samples);
    }

    #[test]
    fn silence_is_not_sent_after_a_short_tail() {
        let mut gate = SilenceGate::default();
        let (loud, quiet) = ([5i16; 960], [0i16; 960]); // 10 ms each at 48 kHz
        assert!(gate.pass(&loud, 48_000));
        let sent = (0..100).filter(|_| gate.pass(&quiet, 48_000)).count();
        assert_eq!(sent, 25, "a quarter of a second of tail, then nothing");
        assert!(gate.pass(&loud, 48_000), "sound starts again at once");
        assert!(gate.pass(&quiet, 48_000));
    }

    #[test]
    fn nothing_plays_until_a_reserve_has_built_up() {
        let mut player = Player::new(48_000);
        let mut out = [1.0f32; 200];
        player.pull(&mut out, 2);
        assert!(out.iter().all(|s| *s == 0.0));
        assert!(player.is_idle());

        // 20 ms is not enough; 80 ms is.
        player.push(1, 1, 48_000, &ramp(960, 100));
        player.pull(&mut out, 2);
        assert!(out.iter().all(|s| *s == 0.0));
        player.push(1, 2, 48_000, &ramp(2880, 1060));
        player.pull(&mut out, 2);
        // Same rate on both sides: the samples come out as they went in.
        assert!((out[0] - 100.0 / 32768.0).abs() < 1e-4, "{}", out[0]);
        assert!((out[1] + 100.0 / 32768.0).abs() < 1e-4);
        assert!(out[2] > out[0], "the ramp continues");
        assert!(!player.is_idle());
    }

    #[test]
    fn rates_are_converted() {
        // 44.1 kHz in, 48 kHz out: one second of input lasts one second.
        let mut player = Player::new(48_000);
        let tone: Vec<i16> = (0..44_100).flat_map(|i| {
            let v = ((i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin() * 8000.0) as i16;
            [v, v]
        }).collect();
        let mut seq = 0;
        let mut produced = 0usize;
        let mut loud = 0usize;
        let mut out = [0f32; 960];
        for chunk in tone.chunks(PACKET_FRAMES * 2) {
            seq += 1;
            player.push(7, seq, 44_100, chunk);
            // Pull at the pace sound arrives (256 frames of 44.1 kHz at a time).
            while produced * 44_100 < seq as usize * PACKET_FRAMES * 48_000 {
                player.pull(&mut out, 2);
                produced += out.len() / 2;
                loud += out.chunks(2).filter(|f| f[0] != 0.0 || f[1] != 0.0).count();
            }
        }
        // Everything but the start-up reserve was played, without running dry.
        assert!(loud > 48_000 * 85 / 100, "{loud}");
        assert!(out.iter().all(|s| s.abs() <= 0.26));
    }

    #[test]
    fn late_duplicates_are_dropped_and_restarts_accepted() {
        let mut player = Player::new(48_000);
        player.push(1, 10, 48_000, &ramp(100, 0));
        player.push(1, 10, 48_000, &ramp(100, 0));
        player.push(1, 9, 48_000, &ramp(100, 0));
        assert_eq!(player.sources[&1].frames.len(), 100);
        player.push(1, 12, 48_000, &ramp(100, 0)); // 11 was lost: carry on
        assert_eq!(player.sources[&1].frames.len(), 200);
        // The sender started again from the beginning.
        let mut restarted = Player::new(48_000);
        restarted.push(1, 50_000, 48_000, &ramp(100, 0));
        restarted.push(1, 1, 48_000, &ramp(100, 0));
        assert_eq!(restarted.sources[&1].frames.len(), 200);
        // Nonsense is ignored.
        player.push(2, 1, 5, &ramp(100, 0));
        player.push(3, 1, 48_000, &[1]);
        assert_eq!(player.sources.len(), 1);
    }

    #[test]
    fn a_backlog_is_trimmed_and_running_dry_rebuilds_the_reserve() {
        let mut player = Player::new(48_000);
        // Half a second arrives at once (the network hiccuped).
        for seq in 1..=94 {
            player.push(1, seq, 48_000, &ramp(PACKET_FRAMES, 1));
        }
        let waiting = player.sources[&1].frames.len();
        assert!(waiting <= 48_000 * 200 / 1000, "kept close to live: {waiting}");

        // Play it all; the reserve empties and playback stops cleanly.
        let mut out = [0f32; 9600];
        for _ in 0..4 {
            player.pull(&mut out, 2);
        }
        assert!(out.iter().all(|s| *s == 0.0));
        assert!(!player.sources[&1].playing);
        // A little sound is not played until the reserve is back.
        player.push(1, 95, 48_000, &ramp(PACKET_FRAMES, 1));
        player.pull(&mut out, 2);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn two_computers_are_mixed_and_quiet_ones_forgotten() {
        let mut player = Player::new(48_000);
        let block: Vec<i16> = std::iter::repeat([8192i16, 0]).take(4800).flatten().collect();
        player.push(1, 1, 48_000, &block);
        player.push(2, 1, 48_000, &block);
        let mut out = [0f32; 20];
        player.pull(&mut out, 2);
        assert!((out[0] - 0.5).abs() < 1e-3 && out[1] == 0.0, "both left channels add up");
        let mut mono = [0f32; 10];
        player.pull(&mut mono, 1);
        assert!((mono[0] - 0.25).abs() < 1e-3, "mono output averages left and right of each");

        let mut long = vec![0f32; 48_000];
        for _ in 0..8 {
            player.pull(&mut long, 2);
        }
        assert!(player.is_idle(), "after three quiet seconds the senders are forgotten");
    }

    #[test]
    fn loud_mixes_are_limited() {
        let mut player = Player::new(48_000);
        let block: Vec<i16> = std::iter::repeat([i16::MAX, i16::MIN]).take(4800).flatten().collect();
        for source in 0..3 {
            player.push(source, 1, 48_000, &block);
        }
        let mut out = [0f32; 20];
        player.pull(&mut out, 2);
        assert_eq!((out[0], out[1]), (1.0, -1.0));
    }
}
