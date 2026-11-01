//! osu!(stable) skins: skin.ini, mania images, fonts and hitsound files.
//!
//! Like the beatmap library, this only reads from the skin folder. Nothing in it
//! is changed or deleted.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Result};
use image::RgbaImage;
use tiny_skia::{IntSize, Pixmap};

pub type Rgba = [u8; 4];

/// A decoded skin image with premultiplied alpha.
pub struct Image {
    px: RgbaImage,
    hd: bool, // came from an @2x file
}

impl Image {
    /// Size in @1x skin pixels.
    pub fn size(&self) -> (f32, f32) {
        let d = if self.hd { 2.0 } else { 1.0 };
        (self.px.width() as f32 / d, self.px.height() as f32 / d)
    }

    /// Resized copy ready to draw. `tint` multiplies the colour.
    pub fn pixmap(&self, w: f32, h: f32, tint: Option<Rgba>) -> Option<Pixmap> {
        let (w, h) = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);
        if w > 8192 || h > 8192 {
            return None;
        }
        let mut img = if (w, h) == self.px.dimensions() {
            self.px.clone()
        } else {
            image::imageops::resize(&self.px, w, h, image::imageops::FilterType::Triangle)
        };
        for p in img.pixels_mut() {
            if let Some(t) = tint {
                for i in 0..4 {
                    p[i] = (p[i] as u32 * t[i] as u32 / 255) as u8;
                }
            }
            // Resampling can round a colour channel above alpha, which isn't valid premultiplied data.
            let a = p[3];
            p[0] = p[0].min(a);
            p[1] = p[1].min(a);
            p[2] = p[2].min(a);
        }
        Pixmap::from_vec(img.into_raw(), IntSize::from_wh(w, h)?)
    }
}

fn load_image(path: &Path, hd: bool) -> Option<Image> {
    let bytes = fs::read(path).ok()?;
    let mut px = image::load_from_memory(&bytes).ok()?.into_rgba8();
    for p in px.pixels_mut() {
        let a = p[3] as u32;
        for i in 0..3 {
            p[i] = ((p[i] as u32 * a + 127) / 255) as u8;
        }
    }
    if px.width() == 0 || px.height() == 0 {
        return None;
    }
    Some(Image { px, hd })
}

/// One `[Mania]` block of skin.ini, for a single key count.
#[derive(Default)]
pub struct ManiaConfig {
    pub keys: usize,
    values: HashMap<String, String>, // keys lowercased
}

impl ManiaConfig {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.values.get(&k.to_ascii_lowercase()).map(|s| s.as_str()).filter(|s| !s.is_empty())
    }
    pub fn num(&self, k: &str) -> Option<f32> {
        self.get(k)?.parse().ok()
    }
    pub fn list(&self, k: &str) -> Vec<f32> {
        self.get(k)
            .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
            .unwrap_or_default()
    }
    pub fn colour(&self, k: &str) -> Option<Rgba> {
        parse_colour(self.get(k)?)
    }
}

fn parse_colour(v: &str) -> Option<Rgba> {
    let n: Vec<u8> = v.split(',').filter_map(|s| s.trim().parse::<f32>().ok()).map(|f| f.clamp(0.0, 255.0) as u8).collect();
    match n.len() {
        3 => Some([n[0], n[1], n[2], 255]),
        4.. => Some([n[0], n[1], n[2], n[3]]),
        _ => None,
    }
}

pub struct Skin {
    pub name: String,
    files: HashMap<String, PathBuf>, // lowercase relative path using '/', with extension
    general: HashMap<String, String>,
    fonts: HashMap<String, String>,
    mania: Vec<ManiaConfig>,
}

impl Skin {
    pub fn load(dir: &Path) -> Result<Skin> {
        if !dir.is_dir() {
            return Err(anyhow!("skin folder {} not found", dir.display()));
        }
        let mut files = HashMap::new();
        index_files(dir, dir, 0, &mut files);

        let mut skin = Skin {
            name: dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            files,
            general: HashMap::new(),
            fonts: HashMap::new(),
            mania: Vec::new(),
        };
        if let Some(ini) = skin.files.get("skin.ini") {
            let text = String::from_utf8_lossy(&fs::read(ini)?).into_owned();
            skin.parse_ini(&text);
        }
        if let Some(n) = skin.general.get("name").filter(|n| !n.is_empty()) {
            skin.name = n.clone();
        }
        Ok(skin)
    }

    fn parse_ini(&mut self, text: &str) {
        let mut section = String::new();
        for line in text.trim_start_matches('\u{feff}').lines() {
            let line = line.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_ascii_lowercase();
                if section == "mania" {
                    self.mania.push(ManiaConfig::default());
                }
                continue;
            }
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
            match section.as_str() {
                "general" => {
                    self.general.insert(k, v);
                }
                "fonts" => {
                    self.fonts.insert(k, v);
                }
                "mania" => {
                    if let Some(m) = self.mania.last_mut() {
                        if k == "keys" {
                            m.keys = v.parse().unwrap_or(0);
                        }
                        m.values.insert(k, v);
                    }
                }
                _ => {}
            }
        }
    }

    pub fn mania(&self, keys: usize) -> Option<&ManiaConfig> {
        self.mania.iter().find(|m| m.keys == keys)
    }

    /// The skin.ini `Version`. Skins without one are treated as 1.0, like osu! does.
    pub fn version(&self) -> f32 {
        match self.general.get("version").map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "latest" => 999.0,
            Some(v) => v.parse().unwrap_or(1.0),
            None => 1.0,
        }
    }

    /// Frames per second for animated elements.
    pub fn animation_fps(&self) -> f32 {
        self.general
            .get("animationframerate")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|f| *f > 0.0)
            .unwrap_or(60.0)
    }

    /// A `[Fonts]` prefix such as `ComboPrefix`, which may point into a subfolder.
    pub fn font_prefix(&self, key: &str) -> String {
        self.fonts
            .get(&key.to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| "score".into())
    }

    pub fn font_overlap(&self, key: &str) -> f32 {
        self.fonts.get(&key.to_ascii_lowercase()).and_then(|v| v.parse().ok()).unwrap_or(0.0)
    }

    fn find(&self, name: &str, ext: &str) -> Option<&PathBuf> {
        self.files.get(&format!("{}.{ext}", normalize(name)))
    }

    /// A single image, preferring the @2x version. `name` has no extension.
    pub fn image(&self, name: &str) -> Option<Image> {
        let name = name.trim();
        let name = name.strip_suffix(".png").or_else(|| name.strip_suffix(".PNG")).unwrap_or(name);
        if let Some(p) = self.find(&format!("{name}@2x"), "png") {
            if let Some(img) = load_image(p, true) {
                return Some(img);
            }
        }
        self.find(name, "png").and_then(|p| load_image(p, false))
    }

    /// The image, or the first frame of its animation.
    pub fn sprite(&self, name: &str) -> Option<Image> {
        self.image(name).or_else(|| self.image(&format!("{name}-0")))
    }

    /// All frames of an animated element (`name-0`, `name-1`, ...), or the single image.
    pub fn animation(&self, name: &str) -> Vec<Image> {
        let mut frames = Vec::new();
        while let Some(img) = self.image(&format!("{name}-{}", frames.len())) {
            frames.push(img);
        }
        if frames.is_empty() {
            frames.extend(self.image(name));
        }
        frames
    }

    /// A sound file such as `soft-hitclap`, in any format osu! accepts.
    pub fn sound(&self, name: &str) -> Option<PathBuf> {
        ["wav", "ogg", "mp3"].iter().find_map(|e| self.find(name, e)).cloned()
    }
}

fn normalize(name: &str) -> String {
    name.trim().replace('\\', "/").trim_start_matches("./").to_lowercase()
}

fn index_files(root: &Path, dir: &Path, depth: u32, out: &mut HashMap<String, PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if depth < 3 {
                index_files(root, &p, depth + 1, out);
            }
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.insert(normalize(&rel.to_string_lossy()), p);
        }
    }
}

/// Skin folders in the osu! Skins folder and in omrdr's own skins folder.
pub fn list(osu_dir: &Path, own_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut roots = vec![own_dir.to_path_buf()];
    if !osu_dir.as_os_str().is_empty() {
        roots.push(osu_dir.join("Skins"));
    }
    for root in roots {
        let Ok(rd) = fs::read_dir(&root) else { continue };
        out.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    out.sort_by_key(|p| display_name(p).to_lowercase());
    out
}

pub fn display_name(dir: &Path) -> String {
    dir.file_name().map(|n| n.to_string_lossy().trim().to_string()).unwrap_or_default()
}

/// The skin selected in osu!, from the most recently saved osu!.<user>.cfg.
pub fn current_osu_skin(osu_dir: &Path) -> Option<PathBuf> {
    let mut cfgs: Vec<_> = fs::read_dir(osu_dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_lowercase();
            n.starts_with("osu!.") && n.ends_with(".cfg") && n != "osu!.cfg"
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    cfgs.sort();
    for (_, cfg) in cfgs.iter().rev() {
        let Ok(text) = fs::read_to_string(cfg) else { continue };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            if k.trim() == "Skin" {
                let dir = osu_dir.join("Skins").join(v.trim());
                if dir.is_dir() {
                    return Some(dir);
                }
                // Skin names can have trailing spaces the cfg trimmed away.
                let wanted = v.trim().to_lowercase();
                return list(osu_dir, Path::new(""))
                    .into_iter()
                    .find(|p| display_name(p).to_lowercase() == wanted);
            }
        }
    }
    None
}

/// Unpacks an .osk into `dest_root/<name>/` and returns that folder.
pub fn import_osk(osk: &Path, dest_root: &Path) -> Result<PathBuf> {
    let stem = osk.file_stem().map(|s| s.to_string_lossy().trim().to_string()).unwrap_or_default();
    let stem = if stem.is_empty() { "skin".to_string() } else { stem };
    let dest = dest_root.join(stem);
    let mut zip = zip::ZipArchive::new(fs::File::open(osk)?)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else { continue };
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        std::io::copy(&mut entry, &mut fs::File::create(&out)?)?;
    }
    Ok(dest)
}
