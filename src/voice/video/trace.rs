//! Optional, local aggregate media measurements. No packets, names or addresses are logged.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(crate) struct Trace {
    name: &'static str,
    stages: &'static [&'static str],
    at: Instant,
    count: [u32; 6],
    elapsed: [Duration; 6],
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
    pub(crate) fn new(name: &'static str, stages: &'static [&'static str]) -> Option<Self> {
        path()?;
        Some(Self {
            name,
            stages,
            at: Instant::now(),
            count: [0; 6],
            elapsed: [Duration::ZERO; 6],
        })
    }

    pub(crate) fn record(&mut self, stage: usize, duration: Duration) {
        self.count[stage] += 1;
        self.elapsed[stage] += duration;
    }

    pub(crate) fn flush(&mut self, now: Instant) {
        let window = now.saturating_duration_since(self.at);
        if window < Duration::from_secs(2) {
            return;
        }
        if let Some(path) = path()
            && let Ok(mut output) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let mut row = String::from(self.name);
            use std::fmt::Write as _;
            for (i, stage) in self.stages.iter().enumerate() {
                let fps = f64::from(self.count[i]) / window.as_secs_f64();
                let ms = 1000.0 * self.elapsed[i].as_secs_f64() / f64::from(self.count[i].max(1));
                let _ = write!(row, " {stage}={fps:.1}/s");
                if self.elapsed[i] != Duration::ZERO {
                    let _ = write!(row, "({ms:.1}ms)");
                }
            }
            row.push('\n');
            let _ = output.write_all(row.as_bytes());
        }
        self.at = now;
        self.count.fill(0);
        self.elapsed.fill(Duration::ZERO);
    }
}
