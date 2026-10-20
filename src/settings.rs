use std::{fs, path::PathBuf};
use serde::{Deserialize, Serialize};

const FILE: &str = "settings.json";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub mania_speed: f32, // osu!mania scroll speed, 1 to 40
    pub use_sv: bool,     // follow the map's SV changes
    pub show_judgements: bool,
    pub column_width: f32, // pixels at 720p
    pub hit_position: f32, // hit line as a fraction of the height
    pub quality_crf: u32,  // x264 quality, lower is better
    pub music_volume: f32,
    pub show_combo: bool,
    pub show_accuracy: bool,
    pub show_hit_error: bool,
    pub show_progress: bool,
    pub osu_client_id: String,
    pub osu_client_secret: String,
    pub show_health: bool,
    pub use_osu_songs: bool, // look in the osu!(stable) Songs folder before downloading
    pub osu_dir: PathBuf,    // osu! install folder, empty if not set
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            data_dir: std::env::current_dir().unwrap_or_default(),
            width: 1280,
            height: 720,
            fps: 60,
            mania_speed: 20.0,
            use_sv: true,
            show_judgements: true,
            column_width: 64.0,
            hit_position: 0.83,
            quality_crf: 18,
            music_volume: 1.0,
            show_combo: true,
            show_accuracy: true,
            show_hit_error: true,
            show_progress: true,
            osu_client_id: String::new(),
            osu_client_secret: String::new(),
            show_health: true,
            use_osu_songs: true,
            osu_dir: crate::library::detect_osu_dir().unwrap_or_default(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        fs::read_to_string(FILE)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = fs::write(FILE, s);
        }
    }

    pub fn replays_dir(&self) -> PathBuf {
        self.data_dir.join("replays")
    }
    pub fn beatmaps_dir(&self) -> PathBuf {
        self.data_dir.join("beatmaps")
    }
    pub fn output_dir(&self) -> PathBuf {
        self.data_dir.join("output")
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [self.replays_dir(), self.beatmaps_dir(), self.output_dir()] {
            fs::create_dir_all(d)?;
        }
        Ok(())
    }
}