//! Builds the hitsound track: a sample for every key press, mixed into raw
//! PCM that ffmpeg layers over the music.
//!
//! Like osu!mania, a press plays the sounds of the nearest note in its column,
//! even when the press is a miss or an empty tap.

use std::{
    collections::HashMap,
    env, fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
};

use anyhow::Result;

use crate::{
    beatmap::{Beatmap, HitObject, TimingPoint},
    library::MapSource,
    replay::Frame,
    settings::Settings,
    skin::Skin,
};

pub const RATE: u32 = 44100;

type Clip = Rc<Vec<f32>>; // interleaved stereo at RATE

/// Decodes any audio file ffmpeg understands.
fn decode(path: &Path) -> Option<Clip> {
    let mut cmd = Command::new(crate::render::ffmpeg_path());
    cmd.args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-ac", "2", "-ar", &RATE.to_string(), "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    crate::render::hide_console(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let pcm = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    Some(Rc::new(pcm.collect()))
}

/// Decoded samples from the skin and the beatmap, each loaded once.
struct Bank<'a> {
    skin: Option<&'a Skin>,
    map: &'a MapSource,
    cache: HashMap<String, Option<Clip>>,
}

impl Bank<'_> {
    /// A file from the beatmap. `name` may lack an extension.
    fn map(&mut self, name: &str) -> Option<Clip> {
        let key = format!("map:{}", name.to_lowercase());
        if let Some(c) = self.cache.get(&key) {
            return c.clone();
        }
        let has_ext = Path::new(name).extension().is_some();
        let names: Vec<String> = if has_ext {
            vec![name.to_string()]
        } else {
            ["wav", "ogg", "mp3"].iter().map(|e| format!("{name}.{e}")).collect()
        };
        let clip = names.iter().find_map(|n| {
            let bytes = self.map.read_file(n)?;
            let ext = Path::new(n).extension()?.to_string_lossy().into_owned();
            let tmp = env::temp_dir().join(format!("omrdr_hs_{}.{ext}", std::process::id()));
            fs::write(&tmp, bytes).ok()?;
            let c = decode(&tmp);
            let _ = fs::remove_file(&tmp);
            c
        });
        self.cache.insert(key, clip.clone());
        clip
    }

    fn skin(&mut self, name: &str) -> Option<Clip> {
        let key = format!("skin:{}", name.to_lowercase());
        if let Some(c) = self.cache.get(&key) {
            return c.clone();
        }
        let clip = self.skin.and_then(|s| s.sound(name)).and_then(|p| decode(&p));
        self.cache.insert(key, clip.clone());
        clip
    }
}

fn set_name(s: u8) -> &'static str {
    match s {
        2 => "soft",
        3 => "drum",
        _ => "normal",
    }
}

fn nonzero(v: u8) -> Option<u8> {
    (v != 0).then_some(v)
}

/// The timing point in effect at `t`, with osu!'s 1 ms of leniency.
fn point_at<'a>(pts: &[&'a TimingPoint], t: f64) -> Option<&'a TimingPoint> {
    let i = pts.partition_point(|p| p.time <= t + 1.0);
    (i > 0).then(|| pts[i - 1])
}

/// The clips one note plays and their volume (0 to 1).
fn note_sounds(o: &HitObject, bm: &Beatmap, pts: &[&TimingPoint], st: &Settings, bank: &mut Bank) -> (Vec<Clip>, f32) {
    let tp = point_at(pts, o.time as f64);
    let normal = nonzero(o.sample.normal_set)
        .or_else(|| tp.and_then(|p| nonzero(p.sample_set)))
        .or(nonzero(bm.sample_set))
        .unwrap_or(1);
    let addition = nonzero(o.sample.addition_set).unwrap_or(normal);
    let index = if o.sample.index > 0 { o.sample.index } else { tp.map_or(0, |p| p.sample_index) };
    let volume = if o.sample.volume > 0 { o.sample.volume } else { tp.map_or(100, |p| p.volume) };

    let mut clips = Vec::new();
    if st.beatmap_hitsounds && !o.sample.filename.is_empty() {
        clips.extend(bank.map(&o.sample.filename));
    }
    if clips.is_empty() {
        let mut kinds = vec![(normal, "hitnormal")];
        for (bit, kind) in [(2, "hitwhistle"), (4, "hitfinish"), (8, "hitclap")] {
            if o.sound & bit != 0 {
                kinds.push((addition, kind));
            }
        }
        for (set, kind) in kinds {
            let base = format!("{}-{kind}", set_name(set));
            // Index 0 means "the skin's sound", 1 and up are the map's own files.
            let custom = if st.beatmap_hitsounds && index > 0 {
                let n = if index > 1 { index.to_string() } else { String::new() };
                bank.map(&format!("{base}{n}"))
            } else {
                None
            };
            clips.extend(custom.or_else(|| bank.skin(&base)));
        }
    }
    (clips, volume as f32 / 100.0)
}

/// Writes the hitsound track (raw f32le stereo at `RATE`) to a temp file.
/// `start_ms` is the song time at video time 0. Returns `None` if nothing would play.
#[allow(clippy::too_many_arguments)]
pub fn build(
    bm: &Beatmap,
    frames: &[Frame],
    source: &MapSource,
    skin: Option<&Skin>,
    st: &Settings,
    start_ms: f64,
    rate: f64,
    seconds: f64,
    log: &dyn Fn(String),
) -> Result<Option<PathBuf>> {
    let master = st.hitsound_volume.clamp(0.0, 1.0);
    if master <= 0.0 {
        return Ok(None);
    }
    let mut pts: Vec<&TimingPoint> = bm.timing.iter().collect();
    pts.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));

    let mut columns: Vec<Vec<&HitObject>> = vec![Vec::new(); bm.keys.max(1)];
    for o in &bm.objects {
        if let Some(c) = columns.get_mut(o.column) {
            c.push(o);
        }
    }

    let len = (seconds.max(0.0) * RATE as f64) as usize * 2;
    let mut mix = vec![0.0_f32; len];
    let mut bank = Bank { skin, map: source, cache: HashMap::new() };
    let mut sounds: HashMap<(usize, i32), (Vec<Clip>, f32)> = HashMap::new();
    let mut played = 0usize;

    for (col, objs) in columns.iter().enumerate() {
        if objs.is_empty() {
            continue;
        }
        let mut held = false;
        for f in frames {
            let down = (f.keys >> col) & 1 == 1;
            let pressed = down && !held;
            held = down;
            if !pressed {
                continue;
            }
            let t = f.time as f64;
            let i = objs.partition_point(|o| (o.time as f64) < t);
            let o = match (i.checked_sub(1).map(|j| objs[j]), objs.get(i)) {
                (Some(a), Some(b)) => if t - a.time as f64 <= b.time as f64 - t { a } else { b },
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => continue,
            };
            let (clips, vol) = sounds
                .entry((col, o.time))
                .or_insert_with(|| note_sounds(o, bm, &pts, st, &mut bank));
            let at = ((t - start_ms) / rate / 1000.0 * RATE as f64).round();
            if at < 0.0 {
                continue;
            }
            let at = at as usize * 2;
            let gain = *vol * master;
            for c in clips.iter() {
                if at >= mix.len() {
                    break;
                }
                for (d, s) in mix[at..].iter_mut().zip(c.iter()) {
                    *d += s * gain;
                }
                played += 1;
            }
        }
    }

    if played == 0 {
        log("no hitsounds found in the skin or the beatmap, rendering without them".into());
        return Ok(None);
    }
    let path = env::temp_dir().join(format!("omrdr_hitsounds_{}.raw", std::process::id()));
    let mut out = BufWriter::new(fs::File::create(&path)?);
    for s in mix {
        out.write_all(&s.clamp(-1.0, 1.0).to_le_bytes())?;
    }
    out.flush()?;
    log(format!("mixed {played} hitsounds"));
    Ok(Some(path))
}
