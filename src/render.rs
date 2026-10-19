use std::{
    collections::HashMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
use anyhow::{anyhow, bail, Result};
use tiny_skia::{Color, Paint, Pixmap, Rect, Transform};

use crate::{
    beatmap::Beatmap,
    sim::{Grade, HitEvent, Stats},
    Frame,
};

const W: u32 = 1280;
const H: u32 = 720;
const FPS: u32 = 60;
const COL_W: f32 = 64.0;
const HIT_Y: f32 = 600.0;
const NOTE_H: f32 = 22.0;
const PX_PER_MS: f32 = 0.6;

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

// Seven-segment digits so we need no font yet. Bits: a b c d e f g.
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

struct Scene<'a> {
    bm: &'a Beatmap,
    frames: &'a [Frame],
    events: &'a [HitEvent],
    stats: &'a Stats,
    results: HashMap<(usize, i32), (f64, Grade)>,
    keys: usize,
    x0: f32,
    start_ms: f64,
    end_ms: f64,
}

impl Scene<'_> {
    fn draw(&self, pm: &mut Pixmap, now: f64) {
        let keys = self.keys;
        let x0 = self.x0;
        let field_w = keys as f32 * COL_W;
        let ypos = |t: f64| HIT_Y - ((t - now) as f32) * PX_PER_MS;

        pm.fill(Color::from_rgba8(10, 10, 16, 255));
        fill(pm, x0, 0.0, field_w, H as f32, [20, 20, 30, 255]);
        for c in 0..=keys {
            fill(pm, x0 + c as f32 * COL_W - 1.0, 0.0, 2.0, H as f32, [55, 55, 80, 255]);
        }

        // key states at this moment
        let idx = self.frames.partition_point(|f| (f.time as f64) <= now);
        let held = if idx == 0 { 0 } else { self.frames[idx - 1].keys };
        for c in 0..keys {
            let x = x0 + c as f32 * COL_W;
            if (held >> c) & 1 == 1 {
                fill(pm, x, HIT_Y - 140.0, COL_W, 140.0, [255, 255, 255, 28]);
                fill(pm, x + 4.0, HIT_Y, COL_W - 8.0, 18.0, [255, 255, 255, 255]);
            } else {
                fill(pm, x + 4.0, HIT_Y, COL_W - 8.0, 18.0, [70, 70, 100, 255]);
            }
        }
        fill(pm, x0, HIT_Y - 2.0, field_w, 3.0, [200, 200, 220, 255]);

        // notes
        for o in &self.bm.objects {
            let c = o.column;
            let x = x0 + c as f32 * COL_W;
            let col = column_color(c, keys);
            let res = self.results.get(&(c, o.time)).copied();
            let hit = matches!(res, Some((_, g)) if g != Grade::Miss);
            let missed = matches!(res, Some((_, Grade::Miss)));
            if hit && res.unwrap().0 <= now {
                continue; // already hit, gone
            }
            match o.end_time {
                None => {
                    let y = ypos(o.time as f64);
                    if y < 0.0 || y - NOTE_H > H as f32 {
                        continue;
                    }
                    let col = if missed { [col[0] / 2, col[1] / 2, col[2] / 2, 255] } else { col };
                    fill(pm, x + 4.0, y - NOTE_H, COL_W - 8.0, NOTE_H, col);
                }
                Some(end) => {
                    let y_end = ypos(end as f64);
                    let mut y_head = ypos(o.time as f64);
                    if hit && now >= o.time as f64 {
                        y_head = HIT_Y; // being held: head sticks to the line
                    }
                    if y_end > H as f32 || y_head < 0.0 {
                        continue;
                    }
                    let col = if missed { [col[0] / 2, col[1] / 2, col[2] / 2, 255] } else { col };
                    fill(pm, x + 12.0, y_end, COL_W - 24.0, y_head - y_end, [col[0], col[1], col[2], 150]);
                    fill(pm, x + 4.0, y_head - NOTE_H, COL_W - 8.0, NOTE_H, col);
                }
            }
        }

        // HUD
        let n = self.events.partition_point(|e| e.time <= now);
        let cx = W as f32 / 2.0;

        if n > 0 {
            let e = &self.events[n - 1];
            let age = now - e.time;
            if age < 400.0 {
                let mut col = grade_color(e.grade);
                col[3] = (255.0 * (1.0 - age / 400.0)) as u8;
                fill(pm, cx - 70.0, HIT_Y - 180.0, 140.0, 12.0, col);
            }
        }

        let combo = self.stats.combo[n];
        if combo > 0 {
            number(pm, &combo.to_string(), cx, 250.0, 70.0, [255, 255, 255, 200]);
        }
        number(pm, &format!("{:.2}", self.stats.acc[n] * 100.0), W as f32 - 130.0, 24.0, 36.0, [255, 255, 255, 255]);

        // hit error bar
        let bar_y = H as f32 - 50.0;
        fill(pm, cx - 240.0, bar_y + 8.0, 480.0, 4.0, [60, 60, 80, 255]);
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
                fill(pm, cx + off as f32 * 1.4 - 1.5, bar_y, 3.0, 20.0, col);
            }
        }
        fill(pm, cx - 1.5, bar_y - 4.0, 3.0, 28.0, [255, 255, 255, 255]);

        // progress
        let prog = ((now - self.start_ms) / (self.end_ms - self.start_ms)).clamp(0.0, 1.0) as f32;
        fill(pm, 0.0, 0.0, W as f32 * prog, 4.0, [120, 160, 255, 255]);
    }
}

pub fn render(
    bm: &Beatmap,
    frames: &[Frame],
    events: &[HitEvent],
    stats: &Stats,
    audio: Option<&Path>,
    rate: f64,
    out: &Path,
) -> Result<()> {
    let keys = bm.keys.max(1);
    let first = bm.objects.first().map(|o| o.time).unwrap_or(0);
    let last = bm.objects.iter().map(|o| o.end_time.unwrap_or(o.time)).max().unwrap_or(0);
    let start_ms = ((first - 2000).min(0)) as f64;
    let end_ms = (last + 3000) as f64;
    let total = ((end_ms - start_ms) / rate / 1000.0 * FPS as f64).ceil() as u64;

    let mut results = HashMap::new();
    for e in events {
        results.insert((e.column, e.note_time), (e.time, e.grade));
    }
    let scene = Scene {
        bm,
        frames,
        events,
        stats,
        results,
        keys,
        x0: (W as f32 - keys as f32 * COL_W) / 2.0,
        start_ms,
        end_ms,
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("-s").arg(format!("{W}x{H}"))
        .arg("-framerate").arg(FPS.to_string())
        .args(["-i", "-"]);
    if let Some(a) = audio {
        cmd.arg("-i").arg(a);
        let mut filters = Vec::new();
        if (rate - 1.0).abs() > 1e-6 {
            filters.push(format!("atempo={rate}"));
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
    cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p"])
        .arg(out)
        .stdin(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("could not start ffmpeg ({e}). Is it on your PATH?"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no ffmpeg stdin"))?;
    let mut pm = Pixmap::new(W, H).ok_or_else(|| anyhow!("could not create pixmap"))?;

    for i in 0..total {
        let now = start_ms + i as f64 * 1000.0 / FPS as f64 * rate;
        scene.draw(&mut pm, now);
        stdin.write_all(pm.data())?;
        if i % 120 == 0 {
            eprint!("\rframe {i}/{total}");
        }
    }
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }
    eprintln!("\rdone: {}                    ", out.display());
    Ok(())
}