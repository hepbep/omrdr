use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};
use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use crate::{beatmap, settings::Settings};

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent("omrdr/0.1 (osu! replay renderer)")
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build()
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}

fn load_index(dir: &Path) -> HashMap<String, String> {
    fs::read_to_string(index_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_index(dir: &Path, idx: &HashMap<String, String>) {
    if let Ok(s) = serde_json::to_string_pretty(idx) {
        let _ = fs::write(index_path(dir), s);
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Returns the path of an .osz in the beatmaps folder that contains this exact difficulty,
/// downloading it first if needed.
pub fn ensure_beatmap(md5: &str, st: &Settings, log: &dyn Fn(String)) -> Result<PathBuf> {
    let dir = st.beatmaps_dir();
    fs::create_dir_all(&dir)?;
    let mut index = load_index(&dir);

    if let Some(name) = index.get(md5) {
        let p = dir.join(name);
        if p.is_file() && beatmap::load_from_osz(&p, md5).is_ok() {
            log("beatmap found in the beatmaps folder".into());
            return Ok(p);
        }
    }

    // Also accept .osz files the user dropped in by hand.
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let is_osz = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("osz"))
            .unwrap_or(false);
        if is_osz && beatmap::load_from_osz(&path, md5).is_ok() {
            index.insert(md5.to_string(), file_name(&path));
            save_index(&dir, &index);
            log(format!("beatmap found: {}", file_name(&path)));
            return Ok(path);
        }
    }

    log("beatmap not downloaded yet, looking it up...".into());
    let set_id = find_set_id(md5, st, log)?;
    log(format!("beatmapset id is {set_id}"));

    let dest = dir.join(format!("{set_id}.osz"));
    download_set(set_id, md5, &dest, log)?;
    index.insert(md5.to_string(), file_name(&dest));
    save_index(&dir, &index);
    Ok(dest)
}

fn get_json(ag: &ureq::Agent, url: &str) -> Result<Value> {
    let body = ag.get(url).call()?.into_string()?;
    Ok(serde_json::from_str(&body)?)
}

fn extract_set_id(v: &Value) -> Option<u64> {
    if let Some(first) = v.as_array().and_then(|a| a.first()) {
        return extract_set_id(first);
    }
    for k in ["beatmapset_id", "set_id", "SetID", "beatmapsetId"] {
        if let Some(n) = v.get(k).and_then(|x| x.as_u64()) {
            return Some(n);
        }
    }
    v.get("beatmapset").and_then(|s| s.get("id")).and_then(|x| x.as_u64())
}

fn osu_lookup(ag: &ureq::Agent, md5: &str, st: &Settings) -> Result<u64> {
    let id: u64 = st
        .osu_client_id
        .trim()
        .parse()
        .map_err(|_| anyhow!("the osu! client id must be a number"))?;
    let body = serde_json::json!({
        "client_id": id,
        "client_secret": st.osu_client_secret.trim(),
        "grant_type": "client_credentials",
        "scope": "public",
    })
    .to_string();
    let resp = ag
        .post("https://osu.ppy.sh/oauth/token")
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_string(&body)?
        .into_string()?;
    let v: Value = serde_json::from_str(&resp)?;
    let token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("no access token in the osu! response"))?;

    let url = format!("https://osu.ppy.sh/api/v2/beatmaps/lookup?checksum={md5}");
    let body = ag
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .call()?
        .into_string()?;
    let v: Value = serde_json::from_str(&body)?;
    extract_set_id(&v).ok_or_else(|| anyhow!("no beatmapset id in the osu! response"))
}

fn find_set_id(md5: &str, st: &Settings, log: &dyn Fn(String)) -> Result<u64> {
    let ag = agent();

    if !st.osu_client_id.trim().is_empty() && !st.osu_client_secret.trim().is_empty() {
        match osu_lookup(&ag, md5, st) {
            Ok(id) => return Ok(id),
            Err(e) => log(format!("osu! API lookup failed: {e:#}")),
        }
    }

    // Best effort: these mirror endpoints are not guaranteed to exist or stay the same.
    for base in ["https://catboy.best/api/v2/md5/", "https://osu.direct/api/v2/md5/"] {
        let url = format!("{base}{md5}");
        match get_json(&ag, &url).and_then(|v| {
            extract_set_id(&v).ok_or_else(|| anyhow!("no set id in the response"))
        }) {
            Ok(id) => return Ok(id),
            Err(e) => log(format!("{base} failed: {e:#}")),
        }
    }

    bail!(
        "could not find which beatmapset this replay belongs to. \
         Drop the matching .osz onto the window, or add an osu! API client in the settings."
    )
}

fn try_download(ag: &ureq::Agent, url: &str, part: &Path, md5: &str) -> Result<()> {
    let resp = ag.get(url).call()?;
    {
        let mut out = fs::File::create(part)?;
        let mut reader = resp.into_reader();
        io::copy(&mut reader, &mut out)?;
    }
    beatmap::load_from_osz(part, md5).map(|_| ())
}

fn download_set(set_id: u64, md5: &str, dest: &Path, log: &dyn Fn(String)) -> Result<()> {
    let ag = agent();
    let mirrors = [
        ("Nerinyan", format!("https://api.nerinyan.moe/d/{set_id}")),
        ("catboy.best", format!("https://catboy.best/d/{set_id}")),
        ("osu.direct", format!("https://osu.direct/api/d/{set_id}")),
    ];
    let part = dest.with_extension("osz.part");

    for (name, url) in mirrors {
        log(format!("downloading from {name}..."));
        match try_download(&ag, &url, &part, md5) {
            Ok(()) => {
                fs::rename(&part, dest)?;
                log(format!("saved {}", file_name(dest)));
                return Ok(());
            }
            Err(e) => {
                log(format!("{name} failed: {e:#}"));
                let _ = fs::remove_file(&part);
            }
        }
    }
    bail!(
        "no mirror had the exact version of this map. It may have been updated since the \
         replay was set. Drop the matching .osz onto the window."
    )
}