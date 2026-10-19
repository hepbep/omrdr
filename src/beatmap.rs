use std::{fs, io::Read, path::Path};
use anyhow::{bail, Result};

#[derive(Debug)]
pub struct HitObject {
    pub time: i32,
    pub end_time: Option<i32>, // Some(..) for hold notes
    pub column: usize,
}

#[derive(Debug)]
pub struct TimingPoint {
    pub time: f64,
    pub beat_length: f64, // uninherited: ms per beat, inherited: negative SV multiplier
    pub uninherited: bool,
}

#[derive(Debug, Default)]
pub struct Beatmap {
    pub title: String,
    pub version: String,
    pub audio_file: String,
    pub mode: u8,
    pub keys: usize,
    pub od: f32,
    pub timing: Vec<TimingPoint>,
    pub objects: Vec<HitObject>,
}

pub fn parse_beatmap(text: &str) -> Result<Beatmap> {
    let text = text.trim_start_matches('\u{feff}');
    let mut bm = Beatmap::default();
    let mut section = "";
    let mut raw_objects: Vec<&str> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            continue;
        }
        match section {
            "General" | "Metadata" | "Difficulty" => {
                if let Some((k, v)) = line.split_once(':') {
                    let (k, v) = (k.trim(), v.trim());
                    match k {
                        "AudioFilename" => bm.audio_file = v.to_string(),
                        "Mode" => bm.mode = v.parse().unwrap_or(0),
                        "Title" => bm.title = v.to_string(),
                        "Version" => bm.version = v.to_string(),
                        "CircleSize" => bm.keys = v.parse::<f32>().unwrap_or(4.0) as usize,
                        "OverallDifficulty" => bm.od = v.parse().unwrap_or(5.0),
                        _ => {}
                    }
                }
            }
            "TimingPoints" => {
                let f: Vec<&str> = line.split(',').collect();
                if f.len() >= 2 {
                    bm.timing.push(TimingPoint {
                        time: f[0].trim().parse()?,
                        beat_length: f[1].trim().parse()?,
                        uninherited: f.get(6).map(|s| s.trim() == "1").unwrap_or(true),
                    });
                }
            }
            "HitObjects" => raw_objects.push(line),
            _ => {}
        }
    }

    // Parse objects last, because the column calculation needs the key count.
    let keys = bm.keys.max(1);
    for line in raw_objects {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 4 {
            continue;
        }
        let x: i32 = f[0].parse()?;
        let time: i32 = f[2].parse()?;
        let kind: u32 = f[3].parse()?;
        let column = ((x * keys as i32) / 512).clamp(0, keys as i32 - 1) as usize;
        let end_time = if kind & 128 != 0 {
            // hold note: "endTime:hitSample" in the 6th field
            f.get(5).and_then(|s| s.split(':').next()).and_then(|s| s.parse().ok())
        } else {
            None
        };
        bm.objects.push(HitObject { time, end_time, column });
    }
    bm.objects.sort_by_key(|o| o.time);
    Ok(bm)
}

/// Opens an .osz and returns the difficulty whose file MD5 matches the replay.
pub fn load_from_osz(path: &Path, wanted_md5: &str) -> Result<(Beatmap, String)> {
    let mut zip = zip::ZipArchive::new(fs::File::open(path)?)?;
    let mut seen = Vec::new();

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name()?.to_string();
        if !name.to_lowercase().ends_with(".osu") {
            continue;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let md5 = format!("{:x}", md5::compute(&bytes));
        if md5.eq_ignore_ascii_case(wanted_md5) {
            let text = String::from_utf8_lossy(&bytes);
            return Ok((parse_beatmap(&text)?, name));
        }
        seen.push(format!("{name}  [{md5}]"));
    }
    bail!(
        "no difficulty in the .osz matches md5 {wanted_md5}. Found:\n  {}",
        seen.join("\n  ")
    )
}

/// Copies one file (e.g. the song) out of the .osz.
pub fn extract_audio(osz: &Path, name: &str, dest: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(fs::File::open(osz)?)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let entry_name = entry.name()?.to_string();
        if entry_name.eq_ignore_ascii_case(name) {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            fs::write(dest, bytes)?;
            return Ok(());
        }
    }
    bail!("audio file {name} not found in the .osz")
}