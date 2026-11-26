//! Local aggregate media measurements. Reports contain no frames, names or addresses.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
#[cfg_attr(not(feature = "media-diagnostics"), allow(dead_code))]
pub(crate) enum Metric {
    CaptureArrived,
    CaptureTimeout,
    CaptureUnchanged,
    CapturePaced,
    CaptureBackpressure,
    GpuScaleReadback,
    CpuReadback,
    GpuConvert,
    GpuTransfer,
    Convert,
    Encoded,
    EncoderSkipped,
    EncoderError,
    Keyframes,
    EncodedBytes,
    Preview,
    QueueDrop,
    FrameAge,
    Sent,
    SendDrop,
    WaitKeyframe,
    EncryptFailed,
    PacketFailed,
    SentPackets,
    SentBytes,
    Nacks,
    Plis,
    RtxSent,
    RtxUnavailable,
    RxPackets,
    RxBytes,
    RxRtx,
    RxMarkers,
    RxQueueDrop,
    RxInvalidPacket,
    RxQueueDelay,
    Reassembled,
    DecryptFailed,
    Decoded,
    DecodeEmpty,
    DecodeError,
    NoDecoderSlot,
    Displayed,
    DisplaySkipped,
    AudioRtp,
    AudioTransportOpen,
    AudioDecrypted,
    AudioMixerQueued,
    Count,
}

use Metric::*;
pub(crate) const CAPTURE: &[Metric] = &[
    CaptureArrived,
    CaptureTimeout,
    CaptureUnchanged,
    CapturePaced,
    CaptureBackpressure,
    GpuScaleReadback,
    CpuReadback,
    GpuConvert,
    GpuTransfer,
    Convert,
    Encoded,
    EncoderSkipped,
    EncoderError,
    Keyframes,
    EncodedBytes,
    Preview,
    QueueDrop,
];
pub(crate) const SEND: &[Metric] = &[
    FrameAge,
    Sent,
    SendDrop,
    WaitKeyframe,
    EncryptFailed,
    PacketFailed,
    SentPackets,
    SentBytes,
    Nacks,
    Plis,
    RtxSent,
    RtxUnavailable,
];
#[cfg(feature = "media-diagnostics")]
pub(crate) const NETWORK: &[Metric] = &[
    RxPackets,
    RxBytes,
    RxRtx,
    RxMarkers,
    RxQueueDrop,
    RxInvalidPacket,
];
#[cfg(feature = "media-diagnostics")]
pub(crate) const DECODE: &[Metric] = &[
    RxQueueDelay,
    Reassembled,
    DecryptFailed,
    Decoded,
    DecodeEmpty,
    DecodeError,
    NoDecoderSlot,
    Nacks,
    Plis,
];
#[cfg(feature = "media-diagnostics")]
pub(crate) const DISPLAY: &[Metric] = &[Displayed, DisplaySkipped];
pub(crate) const AUDIO: &[Metric] = &[
    AudioRtp,
    AudioTransportOpen,
    AudioDecrypted,
    AudioMixerQueued,
];

#[derive(Clone, Copy, Default)]
pub(crate) struct Counter {
    pub(crate) count: u64,
    total: Duration,
    pub(crate) peak: Duration,
}

impl Counter {
    pub(crate) fn mean_ms(self) -> f64 {
        self.total.as_secs_f64() * 1_000.0 / self.count.max(1) as f64
    }
}

pub(crate) struct Trace {
    name: &'static str,
    stages: &'static [Metric],
    at: Instant,
    counters: [Counter; Count as usize],
    #[cfg(feature = "media-diagnostics")]
    id: u64,
    #[cfg(feature = "media-diagnostics")]
    stream: Option<u64>,
    #[cfg(feature = "media-diagnostics")]
    details: String,
}

fn path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("RUSTCORD_TRACE_MEDIA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
    .as_ref()
}

impl Trace {
    pub(crate) fn new(name: &'static str, stages: &'static [Metric]) -> Option<Self> {
        if !cfg!(feature = "media-diagnostics") && path().is_none() {
            return None;
        }
        Some(Self {
            name,
            stages,
            at: Instant::now(),
            counters: [Counter::default(); Count as usize],
            #[cfg(feature = "media-diagnostics")]
            id: next_id(),
            #[cfg(feature = "media-diagnostics")]
            stream: None,
            #[cfg(feature = "media-diagnostics")]
            details: String::new(),
        })
    }

    #[cfg(feature = "media-diagnostics")]
    pub(crate) fn for_stream(name: &'static str, stages: &'static [Metric], key: &str) -> Self {
        use std::hash::{BuildHasher, RandomState};
        static HASHER: OnceLock<RandomState> = OnceLock::new();
        let mut trace = Self::new(name, stages).expect("diagnostics enabled");
        trace.stream = Some(HASHER.get_or_init(RandomState::new).hash_one(key));
        trace
    }

    #[cfg(feature = "media-diagnostics")]
    pub(crate) fn details(&mut self, details: String) {
        self.details = details;
    }

    pub(crate) fn hit(&mut self, metric: Metric) {
        self.add(metric, 1);
    }

    pub(crate) fn add(&mut self, metric: Metric, amount: u64) {
        self.counters[metric as usize].count += amount;
    }

    pub(crate) fn record(&mut self, metric: Metric, duration: Duration) {
        let point = &mut self.counters[metric as usize];
        point.count += 1;
        point.total += duration;
        point.peak = point.peak.max(duration);
    }

    pub(crate) fn flush(&mut self, now: Instant) {
        let window = now.saturating_duration_since(self.at);
        let seconds = if cfg!(feature = "media-diagnostics") {
            1
        } else {
            2
        };
        if window >= Duration::from_secs(seconds) {
            self.publish(now, window);
        }
    }

    fn publish(&mut self, now: Instant, window: Duration) {
        if let Some(path) = path()
            && let Ok(mut output) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let mut row = String::from(self.name);
            for &metric in self.stages {
                let point = self.counters[metric as usize];
                let _ = write!(
                    row,
                    " {metric:?}={:.1}/s",
                    point.count as f64 / window.as_secs_f64()
                );
                if point.peak != Duration::ZERO {
                    let _ = write!(row, "({:.1}ms)", point.mean_ms());
                }
            }
            row.push('\n');
            let _ = output.write_all(row.as_bytes());
        }
        #[cfg(feature = "media-diagnostics")]
        {
            let mut history = history();
            if history.samples.len() == 960 {
                history.samples.pop_front();
            }
            history.samples.push_back(Sample {
                id: self.id,
                stream: self.stream,
                name: self.name,
                end: now,
                window,
                details: self.details.clone(),
                metrics: self
                    .stages
                    .iter()
                    .map(|&metric| (metric, self.counters[metric as usize]))
                    .collect(),
            });
        }
        self.at = now;
        self.counters.fill(Counter::default());
    }
}

#[cfg(feature = "media-diagnostics")]
impl Drop for Trace {
    fn drop(&mut self) {
        let now = Instant::now();
        let window = now.saturating_duration_since(self.at);
        if window >= Duration::from_millis(250) && self.counters.iter().any(|c| c.count > 0) {
            self.publish(now, window);
        }
    }
}

#[cfg(feature = "media-diagnostics")]
#[derive(Clone)]
pub(crate) struct Sample {
    id: u64,
    pub(crate) stream: Option<u64>,
    pub(crate) name: &'static str,
    pub(crate) end: Instant,
    pub(crate) window: Duration,
    pub(crate) details: String,
    pub(crate) metrics: Vec<(Metric, Counter)>,
}

#[cfg(feature = "media-diagnostics")]
fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(feature = "media-diagnostics")]
struct History {
    start: Instant,
    wall_start: std::time::SystemTime,
    samples: std::collections::VecDeque<Sample>,
}

#[cfg(feature = "media-diagnostics")]
fn history() -> std::sync::MutexGuard<'static, History> {
    static HISTORY: OnceLock<std::sync::Mutex<History>> = OnceLock::new();
    super::lock(HISTORY.get_or_init(|| {
        std::sync::Mutex::new(History {
            start: Instant::now(),
            wall_start: std::time::SystemTime::now(),
            samples: Default::default(),
        })
    }))
}

#[cfg(feature = "media-diagnostics")]
pub(crate) fn latest() -> Vec<Sample> {
    let history = history();
    let mut seen = std::collections::HashSet::new();
    let mut samples: Vec<_> = history
        .samples
        .iter()
        .rev()
        .filter(|sample| seen.insert((sample.name, sample.stream)))
        .take(20)
        .cloned()
        .collect();
    samples.sort_by_key(|sample| (sample.stream, sample.id));
    samples
}

#[cfg(feature = "media-diagnostics")]
pub(crate) fn clear() {
    let mut history = history();
    history.samples.clear();
    history.start = Instant::now();
    history.wall_start = std::time::SystemTime::now();
}

#[cfg(feature = "media-diagnostics")]
pub(crate) fn report() -> String {
    let (start, wall_start, samples) = {
        let history = history();
        (history.start, history.wall_start, history.samples.clone())
    };
    let wall_ms = wall_start
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut report = format!(
        "Rustcord {} stream diagnostics\n\
         Start (Unix ms UTC): {wall_ms}\n\
         Rates are per second; avg and max are milliseconds.\n\
         Sender 'Sent' counts complete frames accepted by the local UDP socket.\n\
         Viewer 'Displayed' counts distinct pictures uploaded by the stream tile.\n\
         Nacks count requests for packets, including retries.\n",
        env!("CARGO_PKG_VERSION"),
    );
    for sample in samples {
        let ms = sample.end.saturating_duration_since(start).as_millis();
        let _ = write!(
            report,
            "\n+{ms:06} ms {}#{} ({:.2}s)",
            sample.name,
            sample.id,
            sample.window.as_secs_f64()
        );
        if let Some(stream) = sample.stream {
            let _ = write!(report, " viewer={stream:016x}");
        }
        if !sample.details.is_empty() {
            let _ = write!(report, " [{}]", sample.details);
        }
        for (metric, point) in sample.metrics {
            let rate = point.count as f64 / sample.window.as_secs_f64();
            let _ = write!(report, " {metric:?}={}/{rate:.1}/s", point.count);
            if point.peak != Duration::ZERO {
                let _ = write!(
                    report,
                    "(avg={:.2}ms,max={:.2}ms)",
                    point.mean_ms(),
                    point.peak.as_secs_f64() * 1_000.0
                );
            }
        }
    }
    report.push('\n');
    report
}

#[cfg(all(test, feature = "media-diagnostics"))]
mod tests {
    use super::*;

    #[test]
    fn viewer_samples_link_without_exporting_the_stream_key() {
        let key = "fixture-private-stream-key";
        let mut decoder = Trace::for_stream("Test viewer decoder", DECODE, key);
        let mut display = Trace::for_stream("Test viewer display", DISPLAY, key);
        let now = Instant::now();
        decoder.at = now - Duration::from_secs(1);
        display.at = decoder.at;
        decoder.record(Decoded, Duration::from_millis(7));
        decoder.record(Decoded, Duration::from_millis(13));
        display.hit(Displayed);
        decoder.flush(now);
        display.flush(now);

        let samples = latest();
        let decoded = samples
            .iter()
            .find(|sample| sample.name == "Test viewer decoder")
            .unwrap();
        let displayed = samples
            .iter()
            .find(|sample| sample.name == "Test viewer display")
            .unwrap();
        assert_eq!(decoded.stream, displayed.stream);
        let report = report();
        assert!(report.contains("Decoded=2/2.0/s(avg=10.00ms,max=13.00ms)"));
        assert!(report.contains("Displayed=1/1.0/s"));
        assert!(!report.contains(key));
    }
}
