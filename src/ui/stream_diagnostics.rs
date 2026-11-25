use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::voice::video::trace::{self, Metric, Sample};

use super::UiCx;

pub(super) fn show(ctx: &egui::Context, cx: &mut UiCx<'_>) {
    if !cx.view.call_ui.diagnostics_open {
        return;
    }
    ctx.request_repaint_after(Duration::from_secs(1));
    let samples = trace::latest();
    let mut save = false;
    egui::Window::new("Go Live diagnostics")
        .open(&mut cx.view.call_ui.diagnostics_open)
        .default_width(530.0)
        .show(ctx, |ui| {
            ui.label("Share or watch a stream to measure its video. You can close this panel while recording.");
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!samples.is_empty(), |ui| {
                    if ui.button("Copy report").clicked() {
                        ctx.copy_text(trace::report());
                    }
                    save = ui.button("Save report").clicked();
                    if ui.button("Clear samples").clicked() {
                        trace::clear();
                    }
                });
            });
            if samples.is_empty() {
                ui.label("Waiting for the first sample.");
                return;
            }
            egui::ScrollArea::vertical().max_height(415.0).show(ui, |ui| {
                for sample in &samples {
                    sample_row(ui, sample);
                }
            });
        });
    if save {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = std::env::temp_dir().join(format!("rustcord-stream-{stamp}.txt"));
        match std::fs::write(&path, trace::report()) {
            Ok(()) => {
                ctx.copy_text(path.display().to_string());
                cx.view.toast(
                    format!("Report saved. Path copied: {}", path.display()),
                    false,
                );
            }
            Err(error) => cx
                .view
                .toast(format!("Could not save report: {error}"), true),
        }
    }
}

fn sample_row(ui: &mut egui::Ui, sample: &Sample) {
    use Metric::*;

    ui.separator();
    let heading = match sample.stream {
        Some(stream) => format!("{} · {:04x}", sample.name, stream & 0xffff),
        None => sample.name.to_owned(),
    };
    let age = sample.end.elapsed().as_secs_f64();
    let summary = sample
        .metrics
        .iter()
        .filter_map(|&(metric, point)| {
            let label = match metric {
                CaptureArrived => "captured",
                Encoded => "encoded",
                Sent => "sent",
                RxMarkers => "arrival markers",
                Reassembled => "reassembled",
                Decoded => "decoded",
                Displayed => "shown",
                AudioMixerQueued => "audio queued",
                _ => return None,
            };
            Some(format!(
                "{} {:.1}/s",
                label,
                point.count as f64 / sample.window.as_secs_f64()
            ))
        })
        .collect::<Vec<_>>()
        .join(" · ");
    ui.strong(heading);
    if age >= 3.0 {
        ui.small(format!("Last sample {age:.0} s ago"));
    }
    if !sample.details.is_empty() {
        ui.small(&sample.details);
    }
    ui.label(summary);
    ui.push_id((sample.name, sample.stream), |ui| {
        ui.collapsing("Counters and timings", |ui| {
            egui::Grid::new("metrics").striped(true).show(ui, |ui| {
                ui.strong("Metric");
                ui.strong("Rate");
                ui.strong("Avg / max");
                ui.end_row();
                for &(metric, point) in &sample.metrics {
                    ui.label(format!("{metric:?}"));
                    let rate = point.count as f64 / sample.window.as_secs_f64();
                    if matches!(metric, EncodedBytes | SentBytes | RxBytes) {
                        ui.label(format!("{:.0} kbit/s", rate * 8.0 / 1000.0));
                    } else {
                        ui.label(format!("{rate:.1}/s"));
                    }
                    if point.peak.is_zero() {
                        ui.label("—");
                    } else {
                        ui.label(format!(
                            "{:.1} / {:.1} ms",
                            point.mean_ms(),
                            point.peak.as_secs_f64() * 1000.0
                        ));
                    }
                    ui.end_row();
                }
            });
        });
    });
}
