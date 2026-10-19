use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};
use anyhow::{anyhow, bail, Result};
use tiny_skia::{Color, Paint, Pixmap, Rect, Transform};

use crate::{
    beatmap::{Beatmap, TimingPoint},
    replay::Frame,
    settings::Settings,
    sim::{Grade, HitEvent, Stats},
};

type Rgba = [u8; 4];

fn fill(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: Rgba) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        let mut paint = Paint::default();
        paint.set_color_rgba8(c[0], c[1], c[2], c[3]);
        paint.anti_alias = false;
        pm.fill_rect(r, &paint, Transform::identity(), None);
    }
}

// Seven-segment digits so no font is needed yet. Bits: a b c d e f g.
const SEG: [u8; 10] = [63, 6, 91, 79, 102, 109, 125, 7, 127, 111];

fn digit(pm: &mut Pixmap, x: f32, y: f32, h: f32, d: usize, c: Rgba) {
    let w = h * 0.55;
    let t = h * 0.13;
    let half = (h - 3.0 * t) / 2.0;
    let m = SEG[d];
    if m & 1 != 0 { fill(pm, x + t, y, w - 2.0 * t, t, c); }
    if m & 2 != 0 { fill(pm, x + w - t, y + t, t, half, c); }
    if m & 4 != 0 { fill(pm, x + w - t, y + 2.0 * t + half, t, half, c); }
    if m & 8 != 0 { fill(pm, x + t, y + h - t, w - 2.0 * t, t, c); }
    if m & 16 != 0 { fill(pm, x, y + 2.0 * t + half, t, half, c); }
    if m & 32 != 0 { fill(pm, x, y + t, t, half, c); }
    if m & 64 != 0 { fill(pm, x + t, y + t + half, w - 2.0 * t, t, c); }
}

fn text_w(s: &str, h: f32) -> f32 {
    s.chars().map(|ch| if ch == '.' { h * 0.3 } else { h * 0.75 }).sum()
}

fn number(pm: &mut Pixmap, s: &str, cx: f32, y: f32, h: f32, c: Rgba) {
    let mut x = cx - text_w(s, h) / 2.0;
    for ch in s.chars() {
        if ch == '.' {
            let t = h * 0.16;
            fill(pm, x + h * 0.06, y + h - t, t, t, c);
            x += h * 0.3;
        } else {
            if let Some(d) = ch.to_digit(10) {
                digit(pm, x, y, h, d as usize, c);
            }
            x += h * 0.75;
        }
    }
}

fn grade_color(g: Grade) -> Rgba {
    match g {
        Grade::Perfect => [150, 240, 255, 255],
        Grade::Great => [255, 220, 80, 255],
        Grade::Good => [90, 220, 110, 255],
        Grade::Ok => [80, 140, 255, 255],
        Grade::Meh => [200, 140, 90, 255],
        Grade::Miss => [255, 70, 70, 255],
    }
}

fn column_color(c: usize, keys: usize) -> Rgba {
    if c.min(keys - 1 - c) % 2 == 0 { [235, 235, 245, 255] } else { [110, 170, 255, 255] }
}

/// Scroll position segments: (start time, position at start, SV multiplier).
fn build_scroll(bm: &Beatmap, use_sv: bool) -> Vec<(f64, f64, f64)> {
    let mut segs = vec![(-1.0e7_f64, 0.0_f64, 1.0_f64)];
    if !use_sv {
        return segs;
    }
    let mut pts: Vec<&TimingPoint> = bm.timing.iter().collect();
    pts.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
    for p in pts {
        let mult = if p.uninherited {
            1.0 // a red line resets SV
        } else if p.beat_length < 0.0 {
            (-100.0 / p.beat_length).clamp(0.01, 10.0)
        } else {
            1.0
        };
        let &(st, sp, sm) = segs.last().unwrap();
        segs.push((p.time, sp + (p.time - st) * sm, mult));
    }
    segs
}

fn scroll_pos(segs: &[(f64, f64, f64)], t: f64) -> f64 {
    let i = segs.partition_point(|s| s.0 <= t).saturating_sub(1);
    let (st, sp, sm) = segs[i];
    sp + (t - st) * sm
}

struct Scene<'a> {
    bm: &'a Beatmap,
    frames: &'a [Frame],
    events: &'a [HitEvent],
    stats: &'a Stats,
    results: HashMap<(usize, i32), (f64, Grade)>,
    keys: usize,
    w: f32,
    h: f32,
    s: f32,
    x0: f32,
    col_w: f32,
    hit_y: f32,
    note_h: f32,
    px_per_unit: f32,
    scroll: Vec<(f64, f64, f64)>,
    show_combo: bool,
    show_acc: bool,
    show_err: bool,
    show_progress: bool,
    show_judgements: bool,
    start_ms: f64,
    end_ms: f64,
}

impl Scene<'_> {
    fn draw(&self, pm: &mut Pixmap, now: f64) {
        let (s, keys, x0, cw, hy, nh) =
            (self.s, self.keys, self.x0, self.col_w, self.hit_y, self.note_h);
        let (w, h) = (self.w, self.h);
        let field_w = keys as f32 * cw;
        let now_pos = scroll_pos(&self.scroll, now);
        let ypos = |t: f64| {
            hy - ((scroll_pos(&self.scroll, t) - now_pos) as f32) * self.px_per_unit
        };

        pm.fill(Color::from_rgba8(10, 10, 16, 255));
        fill(pm, x0, 0.0, field_w, h, [20, 20, 30, 255]);
        for c in 0..=keys {
            fill(pm, x0 + c as f32 * cw - s, 0.0, 2.0 * s, h, [55, 55, 80, 255]);
        }

        let idx = self.frames.partition_point(|f| (f.time as f64) <= now);
        let held = if idx == 0 { 0 } else { self.frames[idx - 1].keys };
        for c in 0..keys {
            let x = x0 + c as f32 * cw;
            if (held >> c) & 1 == 1 {
                fill(pm, x, hy - 140.0 * s, cw, 140.0 * s, [255, 255, 255, 28]);
                fill(pm, x + 4.0 * s, hy, cw - 8.0 * s, 18.0 * s, [255, 255, 255, 255]);
            } else {
                fill(pm, x + 4.0 * s, hy, cw - 8.0 * s, 18.0 * s, [70, 70, 100, 255]);
            }
        }
        fill(pm, x0, hy - 2.0 * s, field_w, 3.0 * s, [200, 200, 220, 255]);

        for o in &self.bm.objects {
            let c = o.column;
            let x = x0 + c as f32 * cw;
            let col = column_color(c, keys);
            let res = self.results.get(&(c, o.time)).copied();
            let hit = matches!(res, Some((_, g)) if g != Grade::Miss);
            let missed = matches!(res, Some((_, Grade::Miss)));
            if hit && res.unwrap().0 <= now {
                continue;
            }
            let col = if missed { [col[0] / 2, col[1] / 2, col[2] / 2, 255] } else { col };
            match o.end_time {
                None => {
                    let y = ypos(o.time as f64);
                    if y < 0.0 || y - nh > h {
                        continue;
                    }
                    fill(pm, x + 4.0 * s, y - nh, cw - 8.0 * s, nh, col);
                }
                Some(end) => {
                    let y_end = ypos(end as f64);
                    let mut y_head = ypos(o.time as f64);
                    if hit && now >= o.time as f64 {
                        y_head = hy;
                    }
                    if y_end > h || y_head < 0.0 {
                        continue;
                    }
                    fill(pm, x + 12.0 * s, y_end, cw - 24.0 * s, y_head - y_end, [col[0], col[1], col[2], 150]);
                    fill(pm, x + 4.0 * s, y_head - nh, cw - 8.0 * s, nh, col);
                }
            }
        }

        let n = self.events.partition_point(|e| e.time <= now);
        let cx = w / 2.0;

        if n > 0 {
            let e = &self.events[n - 1];
            let age = now - e.time;
            if age < 400.0 {
                let mut col = grade_color(e.grade);
                col[3] = (255.0 * (1.0 - age / 400.0)) as u8;
                fill(pm, cx - 70.0 * s, hy - 180.0 * s, 140.0 * s, 12.0 * s, col);
            }
        }

        if self.show_combo {
            let combo = self.stats.combo[n];
            if combo > 0 {
                number(pm, &combo.to_string(), cx, h * 0.35, 70.0 * s, [255, 255, 255, 200]);
            }
        }
        if self.show_acc {
            number(
                pm,
                &format!("{:.2}", self.stats.acc[n] * 100.0),
                w - 130.0 * s, 24.0 * s, 36.0 * s, [255, 255, 255, 255],
            );
            
        }

        if self.show_judgements {
            let c = self.stats.counts[n];
            let order = [
                Grade::Perfect, Grade::Great, Grade::Good,
                Grade::Ok, Grade::Meh, Grade::Miss,
            ];
            let top = 44.0 * s; // space above is left for a health bar
            let row = 26.0 * s;
            let dh = 20.0 * s;
            for (i, g) in order.iter().enumerate() {
                let y = top + i as f32 * row;
                fill(pm, 20.0 * s, y + 3.0 * s, 14.0 * s, 14.0 * s, grade_color(*g));
                let txt = c[i].to_string();
                let cx = 46.0 * s + text_w(&txt, dh) / 2.0;
                number(pm, &txt, cx, y, dh, [255, 255, 255, 230]);
            }
        }

        if self.show_err {
            let bar_y = h - 50.0 * s;
            fill(pm, cx - 240.0 * s, bar_y + 8.0 * s, 480.0 * s, 4.0 * s, [60, 60, 80, 255]);
            let mut k = n;
            while k > 0 {
                k -= 1;
                let e = &self.events[k];
                let age = now - e.time;
                if age > 4000.0 {
                    break;
                }
                if let Some(off) = e.offset {
                    let mut col = grade_color(e.grade);
                    col[3] = (255.0 * (1.0 - age / 4000.0)) as u8;
                    fill(pm, cx + off as f32 * 1.4 * s - 1.5 * s, bar_y, 3.0 * s, 20.0 * s, col);
                }
            }
            fill(pm, cx - 1.5 * s, bar_y - 4.0 * s, 3.0 * s, 28.0 * s, [255, 255, 255, 255]);
        }

        if self.show_progress {
            let prog = ((now - self.start_ms) / (self.end_ms - self.start_ms)).clamp(0.0, 1.0) as f32;
            fill(pm, 0.0, 0.0, w * prog, 4.0 * s, [120, 160, 255, 255]);
        }
    }
}

fn ffmpeg_path() -> PathBuf {
    let exe = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let mut spots = Vec::new();
    if let Ok(p) = std::env::current_exe() {
        if let Some(d) = p.parent() {
            spots.push(d.join(exe));
        }
    }
    if let Ok(d) = std::env::current_dir() {
        spots.push(d.join(exe));
    }
    spots.into_iter().find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    bm: &Beatmap,
    frames: &[Frame],
    events: &[HitEvent],
    stats: &Stats,
    audio: Option<&Path>,
    rate: f64,
    st: &Settings,
    out: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let keys = bm.keys.max(1);
    let (wi, hi) = (st.width.max(320) & !1, st.height.max(240) & !1);
    let fps = st.fps.clamp(10, 240);
    let s = hi as f32 / 720.0;
    let (w, h) = (wi as f32, hi as f32);

    let first = bm.objects.first().map(|o| o.time).unwrap_or(0);
    let last = bm.objects.iter().map(|o| o.end_time.unwrap_or(o.time)).max().unwrap_or(0);
    let start_ms = ((first - 2000).min(0)) as f64;
    let end_ms = (last + 3000) as f64;
    let total = ((end_ms - start_ms) / rate / 1000.0 * fps as f64).ceil() as u64;

    let mut results = HashMap::new();
    for e in events {
        results.insert((e.column, e.note_time), (e.time, e.grade));
    }
    let col_w = st.column_width * s;
    let scene = Scene {
        bm, frames, events, stats, results, keys,
        w, h, s,
        x0: (w - keys as f32 * col_w) / 2.0,
        col_w,
        hit_y: h * st.hit_position.clamp(0.3, 0.97),
        note_h: 22.0 * s,
        px_per_unit: 0.8375 * h * st.mania_speed.clamp(1.0, 40.0) / 11485.0 / rate as f32,
        scroll: build_scroll(bm, st.use_sv),
        show_combo: st.show_combo,
        show_acc: st.show_accuracy,
        show_err: st.show_hit_error,
        show_progress: st.show_progress,
        show_judgements: st.show_judgements,
        start_ms, end_ms,
    };

    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("-s").arg(format!("{wi}x{hi}"))
        .arg("-framerate").arg(fps.to_string())
        .args(["-i", "-"]);
    if let Some(a) = audio {
        cmd.arg("-i").arg(a);
        let mut filters = Vec::new();
        if (rate - 1.0).abs() > 1e-6 {
            filters.push(format!("atempo={rate}"));
        }
        if (st.music_volume - 1.0).abs() > 1e-3 {
            filters.push(format!("volume={:.3}", st.music_volume.clamp(0.0, 2.0)));
        }
        if start_ms < 0.0 {
            let d = (-start_ms / rate).round() as i64;
            filters.push(format!("adelay={d}|{d}"));
        }
        cmd.args(["-map", "0:v", "-map", "1:a"]);
        if !filters.is_empty() {
            cmd.arg("-af").arg(filters.join(","));
        }
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-shortest"]);
    }
    cmd.args(["-c:v", "libx264", "-preset", "veryfast"])
        .arg("-crf").arg(st.quality_crf.clamp(10, 35).to_string())
        .args(["-pix_fmt", "yuv420p"])
        .arg(out)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // no console window
    }

    let mut child = cmd.spawn().map_err(|e| {
        anyhow!("could not start ffmpeg ({e}). Put ffmpeg.exe next to omrdr.exe or install it on your PATH.")
    })?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no ffmpeg stdin"))?;
    let mut pm = Pixmap::new(wi, hi).ok_or_else(|| anyhow!("could not create the frame buffer"))?;

    for i in 0..total {
        if cancel.load(Ordering::Relaxed) {
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(out);
            bail!("cancelled");
        }
        let now = start_ms + i as f64 * 1000.0 / fps as f64 * rate;
        scene.draw(&mut pm, now);
        if stdin.write_all(pm.data()).is_err() {
            break; // ffmpeg stopped, its message is read below
        }
        if i % 15 == 0 {
            progress(i, total);
        }
    }
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        bail!("ffmpeg failed ({status}): {}", err.trim());
    }
    progress(total, total);
    Ok(())
}