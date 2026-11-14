//! Test hook `RUSTCORD_FRAME_STATS=1` (docs/ARCHITECTURE.md): every 2 s one stderr line with the
//! frames drawn, eframe's time per frame (`cpu_usage`: our passes + egui + tessellation + GL
//! paint, without the vsync wait), our own time (`logic` + `ui`), one sampled tessellation of the
//! current shapes and the sections timed with `scope` / `time` in ms per frame. Sections named
//! `+name` run inside another one (markdown inside chat …) and are left out of `unscoped`.
//! `RUSTCORD_FRAME_STATS=continuous` also repaints every frame (cost of a full repaint).
//! Off, every call is one cached check.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const EVERY: Duration = Duration::from_secs(2);

/// 0 off, 1 on, 2 continuous.
fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("RUSTCORD_FRAME_STATS").as_deref() {
        Ok("continuous") => 2,
        Ok("" | "0") | Err(_) => 0,
        Ok(_) => 1,
    })
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    pass: Option<Instant>,
    frame_nr: u64,
    /// eframe's time per frame and ours (ms).
    frame_ms: Vec<f32>,
    app_ms: Vec<f32>,
    /// (name, total, calls), in first-seen order.
    sections: Vec<(&'static str, Duration, u32)>,
}

thread_local! {
    static STATS: RefCell<Stats> = RefCell::default();
}

pub struct Scope(&'static str, Instant);

impl Drop for Scope {
    fn drop(&mut self) {
        let d = self.1.elapsed();
        STATS.with_borrow_mut(|s| match s.sections.iter_mut().find(|e| e.0 == self.0) {
            Some(e) => (e.1, e.2) = (e.1 + d, e.2 + 1),
            None => s.sections.push((self.0, d, 1)),
        });
    }
}

/// Times the rest of the enclosing block as section `name` (nothing with the hook off).
pub fn scope(name: &'static str) -> Option<Scope> {
    (mode() != 0).then(|| Scope(name, Instant::now()))
}

/// `f` timed as section `name`.
pub fn time<R>(name: &'static str, f: impl FnOnce() -> R) -> R {
    let _t = scope(name);
    f()
}

/// Start of a pass (`App::logic`). `last`: eframe's time of the previous frame.
pub fn begin(ctx: &egui::Context, last: Option<f32>) {
    if mode() == 0 {
        return;
    }
    STATS.with_borrow_mut(|s| {
        if let Some(t) = last.filter(|_| ctx.cumulative_frame_nr() != s.frame_nr) {
            s.frame_ms.push(t * 1000.0);
        }
        s.pass = Some(Instant::now());
    });
}

/// End of a pass (`App::ui`): our time, the report every 2 s, the continuous repaint.
pub fn end(ctx: &egui::Context) {
    if mode() == 0 {
        return;
    }
    let nr = ctx.cumulative_frame_nr();
    let done = STATS.with_borrow_mut(|s| {
        let app = s.pass.take().map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0);
        match s.app_ms.last_mut().filter(|_| nr == s.frame_nr) {
            // A second pass of the same frame (`request_discard`).
            Some(a) => *a += app,
            None => s.app_ms.push(app),
        }
        s.frame_nr = nr;
        let since = *s.since.get_or_insert_with(Instant::now);
        (since.elapsed() >= EVERY).then(|| std::mem::replace(s, Stats { frame_nr: nr, ..Stats::default() }))
    });
    if let Some(s) = done {
        eprintln!("{}", report(ctx, s));
    }
    if mode() == 2 {
        ctx.request_repaint();
    }
}

fn mean_p95(v: &mut [f32]) -> (f32, f32) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_by(f32::total_cmp);
    (v.iter().sum::<f32>() / v.len() as f32, v[(v.len() * 95 / 100).min(v.len() - 1)])
}

fn report(ctx: &egui::Context, mut s: Stats) -> String {
    let secs = s.since.map_or(0.0, |t| t.elapsed().as_secs_f32());
    let n = s.app_ms.len().max(1) as f32;
    let ((fm, fp), (am, ap)) = (mean_p95(&mut s.frame_ms), mean_p95(&mut s.app_ms));
    let (tess, shapes, prims, verts) = tessellate(ctx);
    let mut out = format!(
        "frame-stats: {} frames in {secs:.1} s | frame {fm:.2} ms (p95 {fp:.2}) | app {am:.2} (p95 {ap:.2}) | tess {tess:.2} ({shapes} shapes, {prims} prims, {verts} vtx) | rest {:.2} |",
        s.app_ms.len(),
        fm - am - tess
    );
    let mut scoped = 0.0;
    for (name, total, calls) in &s.sections {
        let ms = total.as_secs_f32() * 1000.0 / n;
        if !name.starts_with('+') {
            scoped += ms;
        }
        let _ = write!(out, " {name} {ms:.2}");
        if *calls as f32 >= 1.5 * n {
            let _ = write!(out, " x{:.0}", *calls as f32 / n);
        }
        out.push(',');
    }
    let _ = write!(out, " unscoped {:.2}", am - scoped);
    out
}

/// One tessellation of the shapes painted so far this pass: (ms, shapes, clip primitives, vertices).
fn tessellate(ctx: &egui::Context) -> (f32, usize, usize, usize) {
    let mut layers: Vec<egui::LayerId> = ctx.memory(|m| m.layer_ids().collect());
    if !layers.contains(&egui::LayerId::background()) {
        layers.push(egui::LayerId::background());
    }
    let shapes: Vec<egui::epaint::ClippedShape> = ctx.graphics(|g| layers.iter().filter_map(|l| g.get(*l)).flat_map(|l| l.all_entries().cloned()).collect());
    let count = shapes.len();
    let t = Instant::now();
    let prims = ctx.tessellate(shapes, ctx.pixels_per_point());
    let ms = t.elapsed().as_secs_f32() * 1000.0;
    let verts = prims.iter().map(|p| if let egui::epaint::Primitive::Mesh(m) = &p.primitive { m.vertices.len() } else { 0 }).sum();
    (ms, count, prims.len(), verts)
}

#[cfg(test)]
mod tests {
    #[test]
    fn mean_and_p95() {
        let mut v: Vec<f32> = (1..=20).rev().map(|i| i as f32).collect();
        assert_eq!(super::mean_p95(&mut v), (10.5, 20.0));
        assert_eq!(super::mean_p95(&mut []), (0.0, 0.0));
    }
}
