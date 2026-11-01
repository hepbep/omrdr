use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
};
use anyhow::{anyhow, bail, Result};
use tiny_skia::{
    BlendMode, Color, FilterQuality, Paint, Pattern, Pixmap, PixmapPaint, Rect, SpreadMode, Transform,
};

use crate::{
    beatmap::{Beatmap, TimingPoint},
    replay::Frame,
    settings::Settings,
    skin::Skin,
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

/// Health over time, built once from the judgements. The gain and loss numbers
/// approximate osu!mania's feel, they are not the exact values.
struct HealthTimeline {
    /// (time_ms, hp after that judgement), sorted by time.
    points: Vec<(f64, f32)>,
}

impl HealthTimeline {
    /// `hp_drain` is the map's HP value (0-10): higher means harsher losses, slower recovery.
    fn new(events: &[HitEvent], hp_drain: f32) -> Self {
        let drain = hp_drain.clamp(0.0, 10.0);
        let loss_scale = 0.7 + drain * 0.06; // HP 5 -> 1.0, HP 10 -> 1.3
        let gain_scale = 1.3 - drain * 0.06; // HP 5 -> 1.0, HP 10 -> 0.7
        let mut sorted: Vec<&HitEvent> = events.iter().collect();
        sorted.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));

        let mut hp = 1.0_f32;
        let mut points = vec![(-1.0e9_f64, 1.0_f32)];
        for e in sorted {
            let d = match e.grade {
                Grade::Perfect => 0.012,
                Grade::Great => 0.010,
                Grade::Good => 0.005,
                Grade::Ok => 0.0,
                Grade::Meh => -0.030,
                Grade::Miss => -0.080,
            };
            hp += if d >= 0.0 { d * gain_scale } else { d * loss_scale };
            hp = hp.clamp(0.0, 1.0);
            points.push((e.time, hp));
        }
        Self { points }
    }

    /// Displayed health (0.0 to 1.0) at a song time, sliding briefly after each change.
    fn at(&self, t: f64) -> f32 {
        let idx = self.points.partition_point(|p| p.0 <= t).saturating_sub(1);
        let (et, to) = self.points[idx];
        let from = if idx == 0 { to } else { self.points[idx - 1].1 };
        let k = ((t - et) / 160.0).clamp(0.0, 1.0) as f32;
        let k = 1.0 - (1.0 - k) * (1.0 - k); // ease out
        from + (to - from) * k
    }
}

fn hp_color(hp: f32) -> Rgba {
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let (r, g, b) = if hp > 0.5 {
        let t = (hp - 0.5) / 0.5;
        (lerp(255.0, 80.0, t), lerp(210.0, 220.0, t), lerp(60.0, 110.0, t))
    } else {
        (255.0, lerp(60.0, 210.0, hp / 0.5), 60.0)
    };
    [r as u8, g as u8, b as u8, 255]
}

/// Vertical health bar, filling from the bottom. `x` is the bar's left edge,
/// `s` is the resolution scale (1.0 at 720p).
fn draw_health(pm: &mut Pixmap, hp: f32, x: f32, h: f32, s: f32) {
    let hp = hp.clamp(0.0, 1.0);
    let bar_w = (12.0 * s).round().max(6.0);
    let bar_h = (h * 0.62).round();
    let y = ((h - bar_h) * 0.5).round();
    let b = (2.0 * s).round().max(2.0);
    let x = x.round();
    fill(pm, x - b, y - b, bar_w + 2.0 * b, bar_h + 2.0 * b, [255, 255, 255, 200]);
    fill(pm, x, y, bar_w, bar_h, [15, 15, 15, 230]);
    let fh = (bar_h * hp).round();
    fill(pm, x, y + bar_h - fh, bar_w, fh, hp_color(hp));
}

fn blit(pm: &mut Pixmap, img: &Pixmap, x: f32, y: f32, opacity: f32) {
    let paint = PixmapPaint { opacity, ..Default::default() };
    pm.draw_pixmap(x.round() as i32, y.round() as i32, img.as_ref(), &paint, Transform::identity(), None);
}

/// Draws `img` stretched to `w` x `h` at (`x`, `y`).
fn blit_to(pm: &mut Pixmap, img: &Pixmap, x: f32, y: f32, w: f32, h: f32, opacity: f32, blend: BlendMode) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let (sx, sy) = (w / img.width() as f32, h / img.height() as f32);
    let paint = PixmapPaint { opacity, blend_mode: blend, quality: FilterQuality::Bilinear };
    pm.draw_pixmap(0, 0, img.as_ref(), &paint, Transform::from_row(sx, 0.0, 0.0, sy, x, y), None);
}

/// Fills a column strip by repeating `img` vertically, lined up so a tile edge sits on `anchor_y`.
fn tile(pm: &mut Pixmap, img: &Pixmap, x: f32, y: f32, h: f32, anchor_y: f32, opacity: f32) {
    let Some(r) = Rect::from_xywh(x.round(), y, img.width() as f32, h) else { return };
    let paint = Paint {
        shader: Pattern::new(
            img.as_ref(),
            SpreadMode::Repeat,
            FilterQuality::Nearest,
            opacity,
            Transform::from_translate(x.round(), anchor_y),
        ),
        anti_alias: false,
        ..Default::default()
    };
    pm.fill_rect(r, &paint, Transform::identity(), None);
}

fn flip_v(mut img: Pixmap) -> Pixmap {
    let row = img.width() as usize * 4;
    let rows = img.height() as usize;
    let data = img.data_mut();
    for y in 0..rows / 2 {
        let (top, bot) = data.split_at_mut((rows - 1 - y) * row);
        top[y * row..(y + 1) * row].swap_with_slice(&mut bot[..row]);
    }
    img
}

/// Digits from a skin font (`score-0.png` ...), scaled to one height.
struct Font {
    glyphs: HashMap<char, Pixmap>,
    overlap: f32,
}

impl Font {
    fn load(skin: &Skin, prefix: &str, overlap: f32, height: f32) -> Option<Font> {
        let zero = skin.image(&format!("{prefix}-0"))?;
        let k = height / zero.size().1;
        let mut glyphs = HashMap::new();
        let names = ('0'..='9').map(|c| (c, c.to_string())).chain([('.', "dot".into()), ('%', "percent".into())]);
        for (ch, n) in names {
            if let Some(img) = skin.image(&format!("{prefix}-{n}")) {
                let (w, h) = img.size();
                glyphs.extend(img.pixmap(w * k, h * k, None).map(|p| (ch, p)));
            }
        }
        if !('0'..='9').all(|c| glyphs.contains_key(&c)) {
            return None;
        }
        Some(Font { glyphs, overlap: overlap * k })
    }

    fn draw(&self, pm: &mut Pixmap, s: &str, cx: f32, y: f32, opacity: f32) {
        let gs: Vec<&Pixmap> = s.chars().filter_map(|c| self.glyphs.get(&c)).collect();
        let total: f32 = gs.iter().map(|g| g.width() as f32 - self.overlap).sum::<f32>() + self.overlap;
        let mut x = cx - total / 2.0;
        for g in gs {
            blit(pm, g, x, y, opacity);
            x += g.width() as f32 - self.overlap;
        }
    }
}

/// One column's skin images, already scaled to the column width.
#[derive(Default)]
struct ColumnArt {
    note: Option<Pixmap>,
    head: Option<Pixmap>,
    body: Option<Pixmap>,
    tail: Option<Pixmap>,
    key_up: Option<Pixmap>,
    key_down: Option<Pixmap>,
    light: Option<Pixmap>,
    lighting_n: Vec<Pixmap>,
    lighting_l: Vec<Pixmap>,
}

/// Playfield layout plus the skin's sprites. Anything the skin lacks is `None`
/// and gets drawn with the plain built-in shapes.
struct Look {
    col_x: Vec<f32>,
    col_w: Vec<f32>,
    x0: f32,
    field_w: f32,
    hit_y: f32,
    note_h: Vec<f32>,
    cols: Vec<ColumnArt>,
    stage: Option<(Vec<Rgba>, Vec<(f32, f32)>, Rgba)>, // column colours, column lines (x, width), line colour
    judgement_line: bool,
    sprites: Vec<(Pixmap, f32, f32)>, // stage left/right, drawn under everything
    hint: Option<(Pixmap, f32, f32)>,
    bottom: Option<(Pixmap, f32, f32)>,
    judgements: Vec<Vec<Pixmap>>, // indexed by Grade
    judge_y: f32,
    combo_y: f32,
    combo_font: Option<Font>,
    score_font: Option<Font>,
    keys_under_notes: bool,
    body_style: u8, // 0 stretch, 1 repeat from the tail, 2 repeat from the head
    frame_ms: f64,
}

/// The default note/key image for a column: 1 and 2 alternate out from the edges, S in the middle.
fn column_kind(c: usize, keys: usize) -> &'static str {
    if keys % 2 == 1 && c == keys / 2 {
        "S"
    } else if c.min(keys - 1 - c) % 2 == 0 {
        "1"
    } else {
        "2"
    }
}

impl Look {
    fn new(skin: Option<&Skin>, keys: usize, w: f32, h: f32, st: &Settings) -> Look {
        let s = h / 720.0;
        let u = h / 480.0; // skin.ini positions are in a 480-tall space
        let k = h / 768.0; // size of one @1x image pixel
        let mc = skin.and_then(|sk| sk.mania(keys));
        let layout = mc.filter(|_| st.skin_layout);

        let widths = layout.map(|m| m.list("ColumnWidth")).unwrap_or_default();
        let spacing = layout.map(|m| m.list("ColumnSpacing")).unwrap_or_default();
        let col_w: Vec<f32> = (0..keys)
            .map(|c| match layout {
                Some(_) => widths.get(c).copied().unwrap_or(30.0).clamp(5.0, 200.0) * u,
                None => st.column_width * s,
            })
            .collect();
        let gaps: Vec<f32> = (0..keys).map(|c| spacing.get(c).copied().unwrap_or(0.0) * u).collect();
        let field_w = col_w.iter().sum::<f32>() + gaps[..keys - 1].iter().sum::<f32>();
        let x0 = ((w - field_w) / 2.0).round();
        let mut col_x = Vec::with_capacity(keys);
        let mut x = x0;
        for c in 0..keys {
            col_x.push(x);
            x += col_w[c] + gaps[c];
        }
        let hit_y = match layout.and_then(|m| m.num("HitPosition")) {
            Some(p) => (p * u).clamp(h * 0.3, h * 0.97),
            None if layout.is_some() => 402.0 * u,
            None => h * st.hit_position.clamp(0.3, 0.97),
        };

        let mut look = Look {
            note_h: vec![22.0 * s; keys],
            cols: (0..keys).map(|_| ColumnArt::default()).collect(),
            stage: None,
            judgement_line: true,
            sprites: Vec::new(),
            hint: None,
            bottom: None,
            judgements: (0..6).map(|_| Vec::new()).collect(),
            judge_y: hit_y - 180.0 * s,
            combo_y: h * 0.35,
            combo_font: None,
            score_font: None,
            keys_under_notes: false,
            body_style: 0,
            frame_ms: 1000.0 / 60.0,
            col_x, col_w, x0, field_w, hit_y,
        };
        let Some(skin) = skin else { return look };

        let get = |key: &str, default: String| -> String {
            mc.and_then(|m| m.get(key)).map(str::to_string).unwrap_or(default)
        };
        let fit = |img: Option<crate::skin::Image>, cw: f32, tint: Option<Rgba>| {
            img.and_then(|i| {
                let (iw, ih) = i.size();
                i.pixmap(cw, ih * cw / iw, tint)
            })
        };
        let natural = |frames: Vec<crate::skin::Image>, tint: Option<Rgba>| -> Vec<Pixmap> {
            frames.iter().filter_map(|i| { let (iw, ih) = i.size(); i.pixmap(iw * k, ih * k, tint) }).collect()
        };

        for c in 0..keys {
            let kind = column_kind(c, keys);
            let cw = look.col_w[c];
            let note_name = get(&format!("NoteImage{c}"), format!("mania-note{kind}"));
            let light_colour = mc.and_then(|m| m.colour(&format!("ColourLight{}", c + 1))).unwrap_or([55, 255, 255, 255]);
            let art = &mut look.cols[c];
            art.note = fit(skin.sprite(&note_name), cw, None);
            art.head = fit(skin.sprite(&get(&format!("NoteImage{c}H"), format!("mania-note{kind}H"))), cw, None);
            // Tails are drawn upside down in downscroll, as osu! does.
            art.tail = fit(skin.sprite(&get(&format!("NoteImage{c}T"), format!("mania-note{kind}T"))), cw, None)
                .map(flip_v);
            art.body = fit(skin.sprite(&get(&format!("NoteImage{c}L"), format!("mania-note{kind}L"))), cw, None);
            // Keys stretch to the column width but keep their height in screen scale, not aspect.
            let key = |name: String| {
                skin.sprite(&name).and_then(|i| i.pixmap(cw, i.size().1 * k, None))
            };
            art.key_up = key(get(&format!("KeyImage{c}"), format!("mania-key{kind}")));
            art.key_down = key(get(&format!("KeyImage{c}D"), format!("mania-key{kind}D")));
            art.light = skin.sprite(&get("StageLight", "mania-stage-light".into())).and_then(|i| {
                i.pixmap(cw, i.size().1 * u, Some(light_colour))
            });
            art.lighting_n = natural(skin.animation(&get("LightingN", "lightingN".into())), Some(light_colour));
            art.lighting_l = natural(skin.animation(&get("LightingL", "lightingL".into())), Some(light_colour));
            // Heads fall back to the plain note, tails to the head, like osu! does.
            if art.head.is_none() {
                art.head = fit(skin.sprite(&note_name), cw, None);
            }
            if art.tail.is_none() {
                art.tail = art.head.clone();
            }
            if let Some(n) = &art.note {
                look.note_h[c] = n.height() as f32;
            }
        }

        if let Some(m) = mc {
            let colours = (0..keys).map(|c| m.colour(&format!("Colour{}", c + 1)).unwrap_or([0, 0, 0, 255])).collect();
            let lw = m.list("ColumnLineWidth");
            let lines = (0..=keys)
                .map(|i| {
                    let x = if i == keys { x0 + field_w } else { look.col_x[i] };
                    (x, lw.get(i).copied().unwrap_or(2.0) * u * 0.5)
                })
                .collect();
            let line_colour = m.colour("ColourColumnLine").unwrap_or([255, 255, 255, 255]);
            look.stage = Some((colours, lines, line_colour));
            look.judgement_line = m.num("JudgementLine").map_or(true, |v| v != 0.0);
            look.keys_under_notes = m.num("KeysUnderNotes").is_some_and(|v| v != 0.0);
        }

        // Combo digits at the skin font's own size. ScorePosition and ComboPosition are
        // centres, measured the way osu! does: 1.6x into a 768-tall stage, offset by 260.
        let combo_prefix = skin.font_prefix("ComboPrefix");
        let combo_h = skin.image(&format!("{combo_prefix}-0")).map_or(70.0 * s, |i| i.size().1 * k);
        look.combo_font = Font::load(skin, &combo_prefix, skin.font_overlap("ComboOverlap"), combo_h);
        look.combo_y = h * 0.35 + (70.0 * s - combo_h) / 2.0;
        let stage_y = |p: f32| (p * 1.6 + 124.0) / 768.0 * h;
        if let Some(m) = layout {
            if let Some(p) = m.num("ScorePosition") {
                look.judge_y = stage_y(p);
            }
            if let Some(p) = m.num("ComboPosition") {
                look.combo_y = stage_y(p) - combo_h / 2.0;
            }
        }
        look.body_style = match mc.and_then(|m| m.num("NoteBodyStyle")) {
            Some(v) => v.clamp(0.0, 2.0) as u8,
            None if skin.version() < 2.5 => 0,
            None => 2,
        };

        if let Some(img) = skin.sprite(&get("StageLeft", "mania-stage-left".into())) {
            let (iw, ih) = img.size();
            let sw = iw * h / ih;
            look.sprites.extend(img.pixmap(sw, h, None).map(|p| (p, x0 - sw, 0.0)));
        }
        if let Some(img) = skin.sprite(&get("StageRight", "mania-stage-right".into())) {
            let (iw, ih) = img.size();
            look.sprites.extend(img.pixmap(iw * h / ih, h, None).map(|p| (p, x0 + field_w, 0.0)));
        }
        if let Some(img) = skin.sprite(&get("StageHint", "mania-stage-hint".into())) {
            let hh = img.size().1 * k;
            look.hint = img.pixmap(field_w, hh, None).map(|p| (p, x0, hit_y - hh / 2.0));
        }
        if let Some(img) = skin.sprite(&get("StageBottom", "mania-stage-bottom".into())) {
            let (iw, ih) = (img.size().0 * k, img.size().1 * k);
            look.bottom = img.pixmap(iw, ih, None).map(|p| (p, x0 + (field_w - iw) / 2.0, h - ih));
        }

        let judgement_names = [
            ("Hit300g", "mania-hit300g"), ("Hit300", "mania-hit300"), ("Hit200", "mania-hit200"),
            ("Hit100", "mania-hit100"), ("Hit50", "mania-hit50"), ("Hit0", "mania-hit0"),
        ];
        for (i, (key, default)) in judgement_names.iter().enumerate() {
            look.judgements[i] = skin
                .animation(&get(key, default.to_string()))
                .iter()
                .filter_map(|img| { let (iw, ih) = img.size(); img.pixmap(iw * k * 0.8, ih * k * 0.8, None) })
                .collect();
        }

        look.score_font = Font::load(skin, &skin.font_prefix("ScorePrefix"), skin.font_overlap("ScoreOverlap"), 36.0 * s);
        look.frame_ms = 1000.0 / skin.animation_fps() as f64;
        look
    }

    /// Frame of an animation that plays once and holds its last frame.
    fn frame<'a>(&self, frames: &'a [Pixmap], age: f64) -> Option<&'a Pixmap> {
        let i = ((age / self.frame_ms) as usize).min(frames.len().checked_sub(1)?);
        frames.get(i)
    }
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
    look: Look,
    px_per_unit: f32,
    scroll: Vec<(f64, f64, f64)>,
    show_combo: bool,
    show_acc: bool,
    show_err: bool,
    show_progress: bool,
    show_judgements: bool,
    show_health: bool,
    health: HealthTimeline,
    start_ms: f64,
    end_ms: f64,
}

impl Scene<'_> {
    fn draw_keys(&self, pm: &mut Pixmap, held: u32) {
        let lk = &self.look;
        let (s, hy) = (self.s, lk.hit_y);
        for c in 0..self.keys {
            let (x, cw) = (lk.col_x[c], lk.col_w[c]);
            let down = (held >> c) & 1 == 1;
            let art = &lk.cols[c];
            let sprite = if down { art.key_down.as_ref().or(art.key_up.as_ref()) } else { art.key_up.as_ref() };
            match sprite {
                Some(k) => blit(pm, k, x, self.h - k.height() as f32, 1.0),
                None if down => fill(pm, x + 4.0 * s, hy, cw - 8.0 * s, 18.0 * s, [255, 255, 255, 255]),
                None => fill(pm, x + 4.0 * s, hy, cw - 8.0 * s, 18.0 * s, [70, 70, 100, 255]),
            }
        }
    }

    fn draw(&self, pm: &mut Pixmap, now: f64) {
        let lk = &self.look;
        let (s, keys, x0, hy) = (self.s, self.keys, lk.x0, lk.hit_y);
        let (w, h) = (self.w, self.h);
        let field_w = lk.field_w;
        let now_pos = scroll_pos(&self.scroll, now);
        let ypos = |t: f64| {
            hy - ((scroll_pos(&self.scroll, t) - now_pos) as f32) * self.px_per_unit
        };

        pm.fill(Color::from_rgba8(10, 10, 16, 255));
        match &lk.stage {
            Some((colours, lines, line_colour)) => {
                for c in 0..keys {
                    fill(pm, lk.col_x[c], 0.0, lk.col_w[c], h, colours[c]);
                }
                for &(x, lw) in lines {
                    fill(pm, x - lw / 2.0, 0.0, lw, hy, *line_colour);
                }
            }
            None => {
                fill(pm, x0, 0.0, field_w, h, [20, 20, 30, 255]);
                for c in 0..=keys {
                    let x = if c == keys { x0 + field_w } else { lk.col_x[c] };
                    fill(pm, x - s, 0.0, 2.0 * s, h, [55, 55, 80, 255]);
                }
            }
        }
        for (img, x, y) in &lk.sprites {
            blit(pm, img, *x, *y, 1.0);
        }

        let idx = self.frames.partition_point(|f| (f.time as f64) <= now);
        let held = if idx == 0 { 0 } else { self.frames[idx - 1].keys };
        for c in 0..keys {
            if (held >> c) & 1 == 1 {
                let (x, cw) = (lk.col_x[c], lk.col_w[c]);
                match &lk.cols[c].light {
                    Some(l) => blit(pm, l, x, hy - l.height() as f32, 1.0),
                    None => fill(pm, x, hy - 140.0 * s, cw, 140.0 * s, [255, 255, 255, 28]),
                }
            }
        }
        if lk.keys_under_notes {
            self.draw_keys(pm, held);
        }
        match &lk.hint {
            Some((img, x, y)) => blit(pm, img, *x, *y, 1.0),
            None if lk.judgement_line => fill(pm, x0, hy - 2.0 * s, field_w, 3.0 * s, [200, 200, 220, 255]),
            None => {}
        }

        let mut holding = Vec::new();
        for o in &self.bm.objects {
            let c = o.column;
            let (x, cw, nh) = (lk.col_x[c], lk.col_w[c], lk.note_h[c]);
            let art = &lk.cols[c];
            let col = column_color(c, keys);
            let res = self.results.get(&(c, o.time)).copied();
            let hit = matches!(res, Some((_, g)) if g != Grade::Miss);
            let missed = matches!(res, Some((_, Grade::Miss)));
            if hit && res.unwrap().0 <= now {
                continue;
            }
            let alpha = if missed { 0.5 } else { 1.0 };
            let col = if missed { [col[0] / 2, col[1] / 2, col[2] / 2, 255] } else { col };
            match o.end_time {
                None => {
                    let y = ypos(o.time as f64);
                    if y < 0.0 || y - nh > h {
                        continue;
                    }
                    match &art.note {
                        Some(n) => blit(pm, n, x, y - nh, alpha),
                        None => fill(pm, x + 4.0 * s, y - nh, cw - 8.0 * s, nh, col),
                    }
                }
                Some(end) => {
                    let y_end = ypos(end as f64);
                    let mut y_head = ypos(o.time as f64);
                    if hit && now >= o.time as f64 {
                        y_head = hy;
                        holding.push(c);
                    }
                    if y_end - nh > h || y_head < 0.0 {
                        continue;
                    }
                    let (Some(head), Some(body)) = (&art.head, &art.body) else {
                        fill(pm, x + 12.0 * s, y_end, cw - 24.0 * s, y_head - y_end, [col[0], col[1], col[2], 150]);
                        fill(pm, x + 4.0 * s, y_head - nh, cw - 8.0 * s, nh, col);
                        continue;
                    };
                    let (tail_y, bot) = (y_end - nh / 2.0, y_head - head.height() as f32 / 2.0);
                    let top = tail_y.max(-(body.height() as f32));
                    if bot > top {
                        match lk.body_style {
                            0 => blit_to(pm, body, x, top, cw, bot - top, alpha, BlendMode::SourceOver),
                            1 => tile(pm, body, x, top, bot - top, tail_y, alpha),
                            _ => tile(pm, body, x, top, bot - top, bot, alpha),
                        }
                    }
                    if let Some(t) = &art.tail {
                        blit(pm, t, x, y_end - t.height() as f32, alpha);
                    }
                    blit(pm, head, x, y_head - head.height() as f32, alpha);
                }
            }
        }

        if !lk.keys_under_notes {
            self.draw_keys(pm, held);
        }
        if let Some((img, x, y)) = &lk.bottom {
            blit(pm, img, *x, *y, 1.0);
        }

        let n = self.events.partition_point(|e| e.time <= now);
        let cx = x0 + field_w / 2.0;

        // Hit lighting: a short flash for each hit, a loop while a hold is held.
        for &c in &holding {
            let frames = &lk.cols[c].lighting_l;
            if !frames.is_empty() {
                let f = &frames[((now / lk.frame_ms) as usize) % frames.len()];
                let lx = lk.col_x[c] + lk.col_w[c] / 2.0 - f.width() as f32 / 2.0;
                blit_to(pm, f, lx, hy - f.height() as f32 / 2.0, f.width() as f32, f.height() as f32, 1.0, BlendMode::Plus);
            }
        }
        let mut k = n;
        while k > 0 {
            k -= 1;
            let e = &self.events[k];
            let age = now - e.time;
            if age > 200.0 {
                break;
            }
            if e.grade == Grade::Miss {
                continue;
            }
            if let Some(f) = lk.frame(&lk.cols[e.column].lighting_n, age) {
                let a = if age < 80.0 { 1.0 } else { 1.0 - (age - 80.0) / 120.0 } as f32;
                let lx = lk.col_x[e.column] + lk.col_w[e.column] / 2.0 - f.width() as f32 / 2.0;
                blit_to(pm, f, lx, hy - f.height() as f32 / 2.0, f.width() as f32, f.height() as f32, a, BlendMode::Plus);
            }
        }

        if n > 0 {
            let e = &self.events[n - 1];
            let age = now - e.time;
            if age < 400.0 {
                let fade = (1.0 - ((age - 250.0) / 150.0).clamp(0.0, 1.0)) as f32;
                match lk.frame(&lk.judgements[e.grade as usize], age) {
                    Some(j) => {
                        // A quick pop that settles, like osu!mania.
                        let pop = 1.0 + 0.25 * (1.0 - (age / 60.0).clamp(0.0, 1.0)) as f32;
                        let (jw, jh) = (j.width() as f32 * pop, j.height() as f32 * pop);
                        blit_to(pm, j, cx - jw / 2.0, lk.judge_y - jh / 2.0, jw, jh, fade, BlendMode::SourceOver);
                    }
                    None => {
                        let mut col = grade_color(e.grade);
                        col[3] = (255.0 * (1.0 - age / 400.0)) as u8;
                        fill(pm, cx - 70.0 * s, lk.judge_y, 140.0 * s, 12.0 * s, col);
                    }
                }
            }
        }

        if self.show_combo {
            let combo = self.stats.combo[n];
            if combo > 0 {
                match &lk.combo_font {
                    Some(f) => f.draw(pm, &combo.to_string(), cx, lk.combo_y, 1.0),
                    None => number(pm, &combo.to_string(), cx, lk.combo_y, 70.0 * s, [255, 255, 255, 200]),
                }
            }
        }
        if self.show_acc {
            let acc = format!("{:.2}", self.stats.acc[n] * 100.0);
            match &lk.score_font {
                Some(f) => f.draw(pm, &format!("{acc}%"), w - 130.0 * s, 24.0 * s, 1.0),
                None => number(pm, &acc, w - 130.0 * s, 24.0 * s, 36.0 * s, [255, 255, 255, 255]),
            }
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
            let cx = w / 2.0;
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

        if self.show_health {
            let right = lk.sprites.iter().map(|(p, x, _)| x + p.width() as f32).fold(x0 + field_w, f32::max);
            draw_health(pm, self.health.at(now), right + 16.0 * s, h, s);
        }

        if self.show_progress {
            let prog = ((now - self.start_ms) / (self.end_ms - self.start_ms)).clamp(0.0, 1.0) as f32;
            fill(pm, 0.0, 0.0, w * prog, 4.0 * s, [120, 160, 255, 255]);
        }
    }
}

pub(crate) fn ffmpeg_path() -> PathBuf {
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

pub(crate) fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // no console window
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Song times (ms) where the video starts and ends.
pub fn timeline(bm: &Beatmap) -> (f64, f64) {
    let first = bm.objects.first().map(|o| o.time).unwrap_or(0);
    let last = bm.objects.iter().map(|o| o.end_time.unwrap_or(o.time)).max().unwrap_or(0);
    (((first - 2000).min(0)) as f64, (last + 3000) as f64)
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    bm: &Beatmap,
    frames: &[Frame],
    events: &[HitEvent],
    stats: &Stats,
    audio: Option<&Path>,
    hitsounds: Option<&Path>,
    skin: Option<&Skin>,
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

    let (start_ms, end_ms) = timeline(bm);
    let total = ((end_ms - start_ms) / rate / 1000.0 * fps as f64).ceil() as u64;

    let mut results = HashMap::new();
    for e in events {
        results.insert((e.column, e.note_time), (e.time, e.grade));
    }
    let health = HealthTimeline::new(events, 5.0);
    let scene = Scene {
        bm, frames, events, stats, results, keys,
        w, h, s,
        look: Look::new(skin, keys, w, h, st),
        px_per_unit: 0.8375 * h * st.mania_speed.clamp(1.0, 40.0) / 11485.0 / rate as f32,
        scroll: build_scroll(bm, st.use_sv),
        show_combo: st.show_combo,
        show_acc: st.show_accuracy,
        show_err: st.show_hit_error,
        show_progress: st.show_progress,
        show_judgements: st.show_judgements,
        show_health: st.show_health,
        health,
        start_ms, end_ms,
    };

    let mut cmd = Command::new(ffmpeg_path());
    cmd.args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg("-s").arg(format!("{wi}x{hi}"))
        .arg("-framerate").arg(fps.to_string())
        .args(["-i", "-"]);
    // Audio graph: the music (sped up and delayed to line up), plus the hitsound track.
    let mut graph = Vec::new();
    let mut labels = Vec::new();
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
        if filters.is_empty() {
            filters.push("anull".into());
        }
        graph.push(format!("[1:a]{}[music]", filters.join(",")));
        labels.push("[music]");
    }
    if let Some(hs) = hitsounds {
        cmd.args(["-f", "f32le", "-ar", &crate::hitsound::RATE.to_string(), "-ac", "2", "-i"]).arg(hs);
        graph.push(format!("[{}:a]anull[hits]", labels.len() + 1));
        labels.push("[hits]");
    }
    if !labels.is_empty() {
        graph.push(match labels.len() {
            1 => format!("{}anull[aout]", labels[0]),
            n => format!("{}amix=inputs={n}:normalize=0:duration=longest[aout]", labels.concat()),
        });
        cmd.arg("-filter_complex").arg(graph.join(";"));
        cmd.args(["-map", "0:v", "-map", "[aout]"]);
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-shortest"]);
    }
    cmd.args(["-c:v", "libx264", "-preset", "veryfast"])
        .arg("-crf").arg(st.quality_crf.clamp(10, 35).to_string())
        .args(["-pix_fmt", "yuv420p"])
        .arg(out)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);

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
