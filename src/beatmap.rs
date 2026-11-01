use std::{fs, io::Read, path::Path};
use anyhow::{bail, Result};

#[derive(Debug)]
pub struct HitObject {
    pub time: i32,
    pub end_time: Option<i32>, // Some(..) for hold notes
    pub column: usize,
    pub sound: u8, // hitsound bits: 2 whistle, 4 finish, 8 clap
    pub sample: Sample,
}

/// A note's own sample settings. Zero means "use the timing point's".
#[derive(Debug, Default)]
pub struct Sample {
    pub normal_set: u8, // 1 normal, 2 soft, 3 drum
    pub addition_set: u8,
    pub index: u32,
    pub volume: u32,
    pub filename: String,
}

#[derive(Debug)]
pub struct TimingPoint {
    pub time: f64,
    pub beat_length: f64, // uninherited: ms per beat, inherited: negative SV multiplier
    pub uninherited: bool,
    pub sample_set: u8,
    pub sample_index: u32,
    pub volume: u32,
}

#[derive(Debug, Default)]
pub struct Beatmap {
    pub title: String,
    pub version: String,
    pub audio_file: String,
    pub mode: u8,
    pub keys: usize,
    pub od: f32,
    pub sample_set: u8, // the map's default set from [General]
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
                        "SampleSet" => bm.sample_set = match v.to_ascii_lowercase().as_str() {
                            "soft" => 2,
                            "drum" => 3,
                            _ => 1,
                        },
                        _ => {}
                    }
                }
            }
            "TimingPoints" => {
                let f: Vec<&str> = line.split(',').collect();
                if f.len() >= 2 {
                    let int = |i: usize| f.get(i).and_then(|s| s.trim().parse::<i64>().ok());
                    bm.timing.push(TimingPoint {
                        time: f[0].trim().parse()?,
                        beat_length: f[1].trim().parse()?,
                        uninherited: f.get(6).map(|s| s.trim() == "1").unwrap_or(true),
                        sample_set: int(3).unwrap_or(0).clamp(0, 3) as u8,
                        sample_index: int(4).unwrap_or(0).max(0) as u32,
                        volume: int(5).unwrap_or(100).clamp(0, 100) as u32,
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
        let sound = f.get(4).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let mut extra: Vec<&str> = f.get(5).map(|s| s.split(':').collect()).unwrap_or_default();
        let end_time = if kind & 128 != 0 {
            // hold note: "endTime:hitSample" in the 6th field
            let end = extra.first().and_then(|s| s.parse().ok());
            if !extra.is_empty() {
                extra.remove(0);
            }
            end
        } else {
            None
        };
        // hitSample: normalSet:additionSet:index:volume:filename
        let num = |i: usize| extra.get(i).and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0);
        let sample = Sample {
            normal_set: num(0).clamp(0, 3) as u8,
            addition_set: num(1).clamp(0, 3) as u8,
            index: num(2).max(0) as u32,
            volume: num(3).clamp(0, 100) as u32,
            filename: extra.get(4).map(|s| s.trim().to_string()).unwrap_or_default(),
        };
        bm.objects.push(HitObject { time, end_time, column, sound, sample });
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