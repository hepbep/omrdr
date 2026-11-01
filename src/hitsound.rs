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

type Clip = usize; // index into Bank::files

/// Decodes any audio file ffmpeg understands to interleaved stereo at RATE.
fn decode(path: &Path) -> Option<Vec<f32>> {
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
    Some(pcm.collect())
}

enum SoundFile {
    Skin(PathBuf),
    Map(Vec<u8>, String), // file bytes and extension
}

/// The sound files a render needs, from the skin and the beatmap. Files are
/// only found here; `decode_all` decodes them afterwards, in parallel.
struct Bank<'a> {
    skin: Option<&'a Skin>,
    map: &'a MapSource,
    found: HashMap<String, Option<Clip>>,
    files: Vec<SoundFile>,
}

impl Bank<'_> {
    fn add(&mut self, key: String, file: Option<SoundFile>) -> Option<Clip> {
        let clip = file.map(|f| {
            self.files.push(f);
            self.files.len() - 1
        });
        self.found.insert(key, clip);
        clip
    }

    /// A file from the beatmap. `name` may lack an extension.
    fn map(&mut self, name: &str) -> Option<Clip> {
        let key = format!("map:{}", name.to_lowercase());
        if let Some(c) = self.found.get(&key) {
            return *c;
        }
        let has_ext = Path::new(name).extension().is_some();
        let names: Vec<String> = if has_ext {
            vec![name.to_string()]
        } else {
            ["wav", "ogg", "mp3"].iter().map(|e| format!("{name}.{e}")).collect()
        };
        let file = names.iter().find_map(|n| {
            let bytes = self.map.read_file(n)?;
            let ext = Path::new(n).extension()?.to_string_lossy().into_owned();
            Some(SoundFile::Map(bytes, ext))
        });
        self.add(key, file)
    }

    fn skin(&mut self, name: &str) -> Option<Clip> {
        let key = format!("skin:{}", name.to_lowercase());
        if let Some(c) = self.found.get(&key) {
            return *c;
        }
        let file = self.skin.and_then(|s| s.sound(name)).map(SoundFile::Skin);
        self.add(key, file)
    }

    /// Decodes every file, a few ffmpeg processes at a time.
    fn decode_all(&self) -> Vec<Option<Vec<f32>>> {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
        let mut out: Vec<Option<Vec<f32>>> = (0..self.files.len()).map(|_| None).collect();
        std::thread::scope(|sc| {
            for (t, slots) in out.chunks_mut(self.files.len().div_ceil(threads).max(1)).enumerate() {
                let first = t * self.files.len().div_ceil(threads).max(1);
                sc.spawn(move || {
                    for (j, slot) in slots.iter_mut().enumerate() {
                        *slot = match &self.files[first + j] {
                            SoundFile::Skin(p) => decode(p),
                            SoundFile::Map(bytes, ext) => {
                                let tmp = env::temp_dir()
                                    .join(format!("omrdr_hs_{}_{}.{ext}", std::process::id(), first + j));
                                let c = fs::write(&tmp, bytes).ok().and_then(|_| decode(&tmp));
                                let _ = fs::remove_file(&tmp);
                                c
                            }
                        };
                    }
                });
            }
        });
        out
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

    let mut bank = Bank { skin, map: source, found: HashMap::new(), files: Vec::new() };
    let mut sounds: HashMap<(usize, i32), (Vec<Clip>, f32)> = HashMap::new();
    let mut hits: Vec<(usize, Clip, f32)> = Vec::new(); // sample position, clip, gain

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
            hits.extend(clips.iter().map(|&c| (at as usize * 2, c, *vol * master)));
        }
    }

    let decoded = bank.decode_all();
    let len = (seconds.max(0.0) * RATE as f64) as usize * 2;
    let mut mix = vec![0.0_f32; len];
    let mut played = 0usize;
    for (at, clip, gain) in hits {
        let Some(pcm) = &decoded[clip] else { continue };
        if at >= mix.len() {
            continue;
        }
        for (d, s) in mix[at..].iter_mut().zip(pcm) {
            *d += s * gain;
        }
        played += 1;
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
