//! Finds beatmaps in an osu!(stable) installation so they don't have to be downloaded.
//!
//! Everything here only reads from the osu! folder. Nothing is copied into it or deleted.

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{anyhow, bail, Result};

use crate::settings::Settings;

/// Where a prepared beatmap's files live.
#[derive(Clone, Debug)]
pub enum MapSource {
    /// A downloaded or dropped .osz file.
    Osz(PathBuf),
    /// An unpacked beatmap folder inside the osu! Songs folder.
    Folder(PathBuf),
}

impl MapSource {
    /// Copies the song out to `dest` so ffmpeg can read it.
    pub fn extract_audio(&self, name: &str, dest: &Path) -> Result<()> {
        match self {
            MapSource::Osz(p) => crate::beatmap::extract_audio(p, name, dest),
            MapSource::Folder(dir) => {
                let src = find_file_ci(dir, name)
                    .ok_or_else(|| anyhow!("audio file {name} not found in {}", dir.display()))?;
                fs::copy(src, dest)?;
                Ok(())
            }
        }
    }
}

fn find_file_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.is_file()
            && p.file_name()
                .map(|f| f.to_string_lossy().eq_ignore_ascii_case(name))
                .unwrap_or(false)
    })
}

/// The usual places osu!(stable) is installed.
pub fn detect_osu_dir() -> Option<PathBuf> {
    let mut spots = Vec::new();
    if let Some(l) = env::var_os("LOCALAPPDATA") {
        spots.push(PathBuf::from(l).join("osu!"));
    }
    spots.push(PathBuf::from(r"C:\Program Files\osu!"));
    spots.push(PathBuf::from(r"C:\Program Files (x86)\osu!"));
    spots.into_iter().find(|p| p.join("osu!.db").is_file() || p.join("Songs").is_dir())
}

/// The Songs folder for an osu! installation. Honours a custom `BeatmapDirectory`
/// from the osu!.<user>.cfg file, otherwise uses `Songs` inside the install folder.
pub fn songs_dir(osu_dir: &Path) -> Option<PathBuf> {
    if osu_dir.as_os_str().is_empty() {
        return None;
    }
    if let Ok(rd) = fs::read_dir(osu_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !(name.starts_with("osu!.") && name.ends_with(".cfg")) {
                continue;
            }
            let Ok(text) = fs::read_to_string(entry.path()) else { continue };
            for line in text.lines() {
                let Some((k, v)) = line.split_once('=') else { continue };
                if k.trim() == "BeatmapDirectory" && !v.trim().is_empty() {
                    let p = PathBuf::from(v.trim());
                    let full = if p.is_absolute() { p } else { osu_dir.join(p) };
                    if full.is_dir() {
                        return Some(full);
                    }
                }
            }
        }
    }
    let d = osu_dir.join("Songs");
    d.is_dir().then_some(d)
}

fn file_md5(p: &Path) -> Option<String> {
    let bytes = fs::read(p).ok()?;
    Some(format!("{:x}", md5::compute(&bytes)))
}

/// Looks for the exact difficulty (by MD5) in the osu! Songs folder.
/// Returns the beatmap folder and the .osu file name inside it.
pub fn find(md5: &str, st: &Settings, log: &dyn Fn(String)) -> Option<(PathBuf, String)> {
    if !st.use_osu_songs || st.osu_dir.as_os_str().is_empty() {
        return None;
    }
    let Some(songs) = songs_dir(&st.osu_dir) else {
        log("osu! Songs folder not found, using downloads instead".into());
        return None;
    };
    let want = md5.to_lowercase();

    // Fast path: osu!.db already lists every map with its hash.
    let db = st.osu_dir.join("osu!.db");
    if db.is_file() {
        match read_db(&db) {
            Ok(map) => {
                if let Some((folder, file)) = map.get(&want) {
                    let dir = songs.join(folder);
                    if file_md5(&dir.join(file)).as_deref() == Some(want.as_str()) {
                        log("beatmap found in your osu! Songs folder".into());
                        return Some((dir, file.clone()));
                    }
                }
            }
            Err(e) => log(format!("could not read osu!.db ({e}), scanning Songs instead")),
        }
    }

    // Slow path: maps added since osu! last wrote its database, or an unreadable database.
    log("not in osu!.db, scanning the Songs folder (the first scan can take a while)...".into());
    let hit = scan(&songs, &want, &st.data_dir);
    if hit.is_some() {
        log("beatmap found in your osu! Songs folder".into());
    } else {
        log("not in your osu! Songs folder, trying the downloaded beatmaps".into());
    }
    hit
}

/// Hashes every .osu in Songs, remembering results (by size and modified time)
/// in songs_cache.json so later scans are quick.
fn scan(songs: &Path, want: &str, cache_dir: &Path) -> Option<(PathBuf, String)> {
    let cache_path = cache_dir.join("songs_cache.json");
    let mut cache: HashMap<String, String> = fs::read_to_string(&cache_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut changed = false;
    let mut found = None;

    'outer: for dir in fs::read_dir(songs).ok()?.flatten() {
        let dp = dir.path();
        let Ok(files) = fs::read_dir(&dp) else { continue };
        for f in files.flatten() {
            let fp = f.path();
            let is_osu = fp
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("osu"))
                .unwrap_or(false);
            if !is_osu {
                continue;
            }
            let Ok(meta) = f.metadata() else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let fname = f.file_name().to_string_lossy().into_owned();
            let key = format!("{}/{}|{}|{}", dir.file_name().to_string_lossy(), fname, meta.len(), mtime);
            let sum = match cache.get(&key) {
                Some(s) => s.clone(),
                None => {
                    let Some(s) = file_md5(&fp) else { continue };
                    cache.insert(key, s.clone());
                    changed = true;
                    s
                }
            };
            if sum == want {
                found = Some((dp.clone(), fname));
                break 'outer;
            }
        }
    }

    if changed {
        if let Ok(s) = serde_json::to_string(&cache) {
            let _ = fs::write(&cache_path, s);
        }
    }
    found
}

// ---------- osu!.db reader (osu!stable) ----------

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p.checked_add(n).filter(|&e| e <= self.b.len());
        let Some(end) = end else { bail!("unexpected end of file") };
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn uleb(&mut self) -> Result<usize> {
        let (mut r, mut shift) = (0usize, 0u32);
        loop {
            let b = self.u8()?;
            r |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                return Ok(r);
            }
            shift += 7;
            if shift > 35 {
                bail!("bad string length");
            }
        }
    }
    fn string(&mut self) -> Result<String> {
        match self.u8()? {
            0 => Ok(String::new()),
            0x0b => {
                let n = self.uleb()?;
                Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
            }
            x => bail!("unexpected string marker {x:#x}"),
        }
    }
}

/// Maps lowercase MD5 -> (beatmap folder name, .osu file name).
fn read_db(path: &Path) -> Result<HashMap<String, (String, String)>> {
    let data = fs::read(path)?;
    let mut r = Rd { b: &data, p: 0 };
    let version = r.i32()?;
    r.skip(4 + 1 + 8)?; // folder count, account unlocked, unlock date
    r.string()?; // player name
    let count = r.i32()?;
    if !(0..5_000_000).contains(&count) {
        bail!("odd beatmap count {count}");
    }
    let mut map = HashMap::with_capacity(count as usize);

    for _ in 0..count {
        if version < 20191106 {
            r.skip(4)?; // entry size, only in older databases
        }
        for _ in 0..6 {
            r.string()?; // artist(+unicode), title(+unicode), creator, difficulty
        }
        r.string()?; // audio file
        let md5 = r.string()?;
        let osu_file = r.string()?;
        if md5.len() != 32 {
            bail!("unexpected layout (hash is {} chars)", md5.len());
        }
        r.skip(1 + 6 + 8)?; // ranked status, circle/slider/spinner counts, last modified
        r.skip(if version >= 20140609 { 16 } else { 4 })?; // AR, CS, HP, OD
        r.skip(8)?; // slider velocity
        if version >= 20140609 {
            for _ in 0..4 {
                let n = r.i32()?;
                if !(0..100_000).contains(&n) {
                    bail!("odd star rating count");
                }
                for _ in 0..n {
                    r.skip(1 + 4)?; // 0x08, mods
                    match r.u8()? {
                        0x0d => r.skip(8)?, // double
                        0x0c => r.skip(4)?, // single (newer databases)
                        x => bail!("unexpected star rating marker {x:#x}"),
                    }
                }
            }
        }
        r.skip(12)?; // drain time, total time, preview time
        let tp = r.i32()?;
        if !(0..1_000_000).contains(&tp) {
            bail!("odd timing point count");
        }
        r.skip(tp as usize * 17)?;
        r.skip(12 + 4 + 2 + 4 + 1)?; // ids, grades, local offset, stack leniency, mode
        r.string()?; // source
        r.string()?; // tags
        r.skip(2)?; // online offset
        r.string()?; // title font
        r.skip(1 + 8 + 1)?; // unplayed, last played, osz2
        let folder = r.string()?;
        r.skip(8 + 5)?; // last checked, 5 ignore/disable flags
        if version < 20140609 {
            r.skip(2)?;
        }
        r.skip(4 + 1)?; // last modification time, mania scroll speed
        map.insert(md5.to_lowercase(), (folder, osu_file));
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(out: &mut Vec<u8>, t: &str) {
        if t.is_empty() {
            out.push(0);
        } else {
            out.push(0x0b);
            assert!(t.len() < 128);
            out.push(t.len() as u8);
            out.extend(t.as_bytes());
        }
    }

    fn entry(out: &mut Vec<u8>, md5: &str, file: &str, folder: &str, single: bool) {
        for t in ["Artist", "", "Title", "", "Mapper", "4K Hard", "audio.mp3"] {
            s(out, t);
        }
        s(out, md5);
        s(out, file);
        out.push(4); // ranked
        out.extend([1u8, 0, 2, 0, 3, 0]);
        out.extend(0i64.to_le_bytes());
        for _ in 0..4 {
            out.extend(5f32.to_le_bytes());
        }
        out.extend(1f64.to_le_bytes());
        for list in 0..4 {
            let n = if list == 3 { 2 } else { 0 };
            out.extend((n as i32).to_le_bytes());
            for _ in 0..n {
                out.push(0x08);
                out.extend(0i32.to_le_bytes());
                if single {
                    out.push(0x0c);
                    out.extend(3f32.to_le_bytes());
                } else {
                    out.push(0x0d);
                    out.extend(3f64.to_le_bytes());
                }
            }
        }
        out.extend([0u8; 12]);
        out.extend(3i32.to_le_bytes()); // timing points
        out.extend([0u8; 51]);
        out.extend([0u8; 12 + 4 + 2 + 4]);
        out.push(3); // mode
        s(out, "");
        s(out, "tag tag");
        out.extend([0u8; 2]);
        s(out, "");
        out.extend([0u8; 1 + 8 + 1]);
        s(out, folder);
        out.extend([0u8; 8 + 5 + 4 + 1]);
    }

    #[test]
    fn reads_a_database_with_both_star_formats() {
        for single in [false, true] {
            let mut db = Vec::new();
            db.extend(20250101i32.to_le_bytes());
            db.extend(1i32.to_le_bytes());
            db.push(1);
            db.extend(0i64.to_le_bytes());
            s(&mut db, "player");
            db.extend(2i32.to_le_bytes());
            entry(&mut db, "0123456789abcdef0123456789ABCDEF", "a.osu", "100 Artist - Title", single);
            entry(&mut db, "ffffffffffffffffffffffffffffffff", "b.osu", "200 Other", single);
            let dir = env::temp_dir().join(format!("omrdr_dbtest_{single}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("osu!.db"), &db).unwrap();
            let m = read_db(&dir.join("osu!.db")).unwrap();
            assert_eq!(m.len(), 2);
            assert_eq!(
                m["0123456789abcdef0123456789abcdef"],
                ("100 Artist - Title".to_string(), "a.osu".to_string())
            );
            assert_eq!(m["ffffffffffffffffffffffffffffffff"].1, "b.osu");
        }
    }

    #[test]
    fn scan_finds_a_map_and_uses_the_cache() {
        let root = env::temp_dir().join("omrdr_scantest");
        let _ = fs::remove_dir_all(&root);
        let songs = root.join("Songs");
        fs::create_dir_all(songs.join("1 Map")).unwrap();
        fs::write(songs.join("1 Map").join("x.osu"), b"hello map").unwrap();
        fs::write(songs.join("1 Map").join("y.osu"), b"other map").unwrap();
        let want = format!("{:x}", md5::compute(b"hello map"));
        let hit = scan(&songs, &want, &root).unwrap();
        assert_eq!(hit.1, "x.osu");
        assert!(root.join("songs_cache.json").is_file());
        assert!(scan(&songs, &"0".repeat(32), &root).is_none());
    }

    #[test]
    fn songs_dir_follows_the_cfg() {
        let root = env::temp_dir().join("omrdr_cfgtest");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Songs")).unwrap();
        fs::create_dir_all(root.join("MyMaps")).unwrap();
        assert_eq!(songs_dir(&root).unwrap(), root.join("Songs"));
        fs::write(root.join("osu!.Leon.cfg"), "Foo = 1\nBeatmapDirectory = MyMaps\n").unwrap();
        assert_eq!(songs_dir(&root).unwrap(), root.join("MyMaps"));
    }
}