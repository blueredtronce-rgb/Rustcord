//! Speaker output shared by the video player and voice messages: the picked device (Voice &
//! Video's output, else the default), F32 only, a ring the cpal callback drains with `try_lock`
//! (never blocks the audio thread), linear resampling to the device rate, volume applied in the
//! callback, and a count of the device frames played (the player's clock).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct AudioOut {
    stream: cpal::Stream,
    ring: Arc<Mutex<VecDeque<f32>>>,
    played: Arc<AtomicU64>,
    volume: Arc<AtomicU32>,
    channels: usize,
    rate: u32,
    /// Source frames per device frame, the position between the last two source frames, and
    /// the last source frame.
    step: f64,
    phase: f64,
    last: [f32; 2],
}

impl AudioOut {
    /// Opens `output` (a device name as in `voice::audio::device`), else the system default, for
    /// stereo `src_rate` Hz input.
    pub fn open(output: Option<&str>, src_rate: u32) -> Result<AudioOut, String> {
        let host = cpal::default_host();
        let picked = output.and_then(|want| host.output_devices().ok()?.find(|d| d.description().is_ok_and(|x| x.name() == want)));
        let dev = picked.or_else(|| host.default_output_device()).ok_or("No speaker")?;
        let sc = dev.default_output_config().map_err(|_| "No speaker")?;
        if sc.sample_format() != cpal::SampleFormat::F32 {
            return Err("Speaker format not supported".into());
        }
        let (channels, rate) = (usize::from(sc.channels().max(1)), sc.sample_rate());
        let ring: Arc<Mutex<VecDeque<f32>>> = Arc::default();
        let played: Arc<AtomicU64> = Arc::default();
        let volume = Arc::new(AtomicU32::new(1f32.to_bits()));
        let (r, p, v) = (ring.clone(), played.clone(), volume.clone());
        let stream = dev
            .build_output_stream(
                sc.config(),
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let vol = f32::from_bits(v.load(Ordering::Relaxed));
                    // A busy lock plays this buffer silent.
                    let mut q = r.try_lock().ok();
                    let mut taken = 0u64;
                    for s in out.iter_mut() {
                        *s = match q.as_mut().and_then(|q| q.pop_front()) {
                            Some(x) => {
                                taken += 1;
                                x * vol
                            }
                            None => 0.0,
                        };
                    }
                    p.fetch_add(taken / channels as u64, Ordering::Relaxed);
                },
                |_| {},
                None,
            )
            .map_err(|_| "Could not open the speaker")?;
        stream.play().map_err(|_| "Could not open the speaker")?;
        Ok(AudioOut { stream, ring, played, volume, channels, rate, step: f64::from(src_rate.max(1)) / f64::from(rate.max(1)), phase: 0.0, last: [0.0; 2] })
    }

    /// Queue interleaved `pcm` with `ch` channels (1 or 2; more: the first two) at the source rate.
    pub fn push(&mut self, pcm: &[f32], ch: usize) {
        let ch = ch.max(1);
        let mut out = Vec::with_capacity(pcm.len() / ch * self.channels * 2);
        for f in pcm.chunks_exact(ch) {
            let cur = [f[0], f[ch.min(2) - 1]];
            while self.phase < 1.0 {
                let t = self.phase as f32;
                let s = [self.last[0] + (cur[0] - self.last[0]) * t, self.last[1] + (cur[1] - self.last[1]) * t];
                match self.channels {
                    1 => out.push((s[0] + s[1]) / 2.0),
                    c => {
                        out.extend_from_slice(&s);
                        out.extend(std::iter::repeat_n(0.0, c - 2));
                    }
                }
                self.phase += self.step;
            }
            self.phase -= 1.0;
            self.last = cur;
        }
        self.ring.lock().unwrap_or_else(|e| e.into_inner()).extend(out);
    }

    /// Device frames queued and not played yet.
    pub fn queued_frames(&self) -> usize {
        self.ring.lock().map_or(0, |q| q.len()) / self.channels
    }

    /// Device frames played since `open` or the last `clear`.
    pub fn played_frames(&self) -> u64 {
        self.played.load(Ordering::Relaxed)
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Drop what is queued (seek, pause) and restart the played count.
    pub fn clear(&mut self) {
        self.ring.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.played.store(0, Ordering::Relaxed);
        (self.phase, self.last) = (0.0, [0.0; 2]);
    }

    /// Stops (false) or restarts the device callback: a paused player leaves the speaker idle.
    pub fn run(&self, on: bool) {
        let _ = if on { self.stream.play() } else { self.stream.pause() };
    }

    /// 0.0 ..= 2.0 (voice messages go to 200 %), applied to what plays from now on.
    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }
}
