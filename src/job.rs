use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use anyhow::{anyhow, bail, Result};

use crate::{
    beatmap::{self, Beatmap},
    fetch, render,
    replay::{self, Replay},
    settings::Settings,
    sim,
};

pub struct Prepared {
    pub replay: Replay,
    pub replay_path: PathBuf,
    pub osz: PathBuf,
    pub bm: Beatmap,
    pub diff_file: String,
    pub split: sim::Split,
}

/// Copies a file into `dir` (unless it is already there) and returns the new path.
pub fn copy_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let name = src.file_name().ok_or_else(|| anyhow!("that path has no file name"))?;
    let dest = dir.join(name);
    let same = matches!(
        (fs::canonicalize(src), fs::canonicalize(&dest)),
        (Ok(a), Ok(b)) if a == b
    );
    if !same {
        fs::copy(src, &dest)?;
    }
    Ok(dest)
}

pub fn prepare(osr: &Path, st: &Settings, log: &dyn Fn(String)) -> Result<Prepared> {
    st.ensure_dirs()?;
    let replay_path = copy_into(osr, &st.replays_dir())?;
    log(format!("replay saved to {}", replay_path.display()));

    let replay = replay::parse_replay(&fs::read(&replay_path)?)?;
    if replay.mode != 3 {
        bail!(
            "this replay is game mode {}. Only osu!mania (mode 3) is supported so far.",
            replay.mode
        );
    }
    log(format!("replay by {}, map hash {}", replay.player, replay.beatmap_md5));

    let osz = fetch::ensure_beatmap(&replay.beatmap_md5, st, log)?;
    let (bm, diff_file) = beatmap::load_from_osz(&osz, &replay.beatmap_md5)?;
    let split = sim::simulate(&bm, &replay.frames);
    Ok(Prepared { replay, replay_path, osz, bm, diff_file, split })
}

pub fn sim_totals(s: &sim::Split) -> [u32; 6] {
    [
        s.tap.geki + s.hold.geki,
        s.tap.n300 + s.hold.n300,
        s.tap.katu + s.hold.katu,
        s.tap.n100 + s.hold.n100,
        s.tap.n50 + s.hold.n50,
        s.tap.miss + s.hold.miss,
    ]
}

pub fn replay_totals(r: &Replay) -> [u32; 6] {
    [
        r.n_geki as u32, r.n300 as u32, r.n_katu as u32,
        r.n100 as u32, r.n50 as u32, r.n_miss as u32,
    ]
}

/// Standard mania accuracy in percent.
pub fn accuracy(t: [u32; 6]) -> f64 {
    let total: u32 = t.iter().sum();
    if total == 0 {
        return 100.0;
    }
    let pts = 300 * (t[0] + t[1]) + 200 * t[2] + 100 * t[3] + 50 * t[4];
    pts as f64 / (300.0 * total as f64) * 100.0
}

pub fn mods_text(m: i32) -> String {
    let names = [
        (1, "NF"), (2, "EZ"), (8, "HD"), (16, "HR"), (32, "SD"),
        (64, "DT"), (256, "HT"), (512, "NC"), (1024, "FL"),
    ];
    let v: Vec<&str> = names.iter().filter(|(b, _)| m & b != 0).map(|(_, n)| *n).collect();
    if v.is_empty() { "none".into() } else { v.join(" ") }
}

pub fn has_difficulty_mods(m: i32) -> bool {
    m & (2 | 16 | 64 | 256 | 512) != 0
}

/// Output file path: the replay's name unless the user typed one. Never overwrites.
pub fn output_path(st: &Settings, replay_path: &Path, custom: &str) -> PathBuf {
    let base = if custom.trim().is_empty() {
        replay_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into())
    } else {
        custom.trim().trim_end_matches(".mp4").to_string()
    };
    let clean: String = base
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    let dir = st.output_dir();
    let mut p = dir.join(format!("{clean}.mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{clean} ({n}).mp4"));
        n += 1;
    }
    p
}

pub fn render_job(
    p: &Prepared,
    st: &Settings,
    out: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    st.ensure_dirs()?;

    let plain = sim::stats(&p.split.events, false);
    let strict = sim::stats(&p.split.events, true);
    let max_of = |s: &sim::Stats| *s.combo.iter().max().unwrap_or(&0) as i64;
    let target = p.replay.max_combo as i64;
    let stats = if (max_of(&plain) - target).abs() <= (max_of(&strict) - target).abs() {
        plain
    } else {
        strict
    };

    let mods = p.replay.mods;
    let rate = if mods & (64 | 512) != 0 { 1.5 } else if mods & 256 != 0 { 0.75 } else { 1.0 };

    let ext = Path::new(&p.bm.audio_file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp3");
    let audio = env::temp_dir().join(format!("omrdr_audio_{}.{ext}", std::process::id()));
    beatmap::extract_audio(&p.osz, &p.bm.audio_file, &audio)?;

    let res = render::render(
        &p.bm, &p.replay.frames, &p.split.events, &stats,
        Some(&audio), rate, st, out, cancel, progress,
    );
    let _ = fs::remove_file(&audio);
    res
}