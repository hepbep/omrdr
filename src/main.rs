use std::{env, fs, io::Cursor};
use anyhow::{anyhow, bail, Result};


mod render;
mod sim;
mod beatmap;
use std::path::Path;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self{data, pos: 0}

    }

    fn take(&mut self, n:usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            bail!("unexpected end of file");
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into()?))
    }


    fn string(&mut self) -> Result<String> {
        match self.u8()? {
            0x00 => Ok(String::new()),
            0x0b => {
                let mut len = 0usize;
                let mut shift = 0;
                loop {
                    let b = self.u8()?;
                    len |= ((b & 0x7f) as usize) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
            }
            other => bail!("bad string marker {other:#x}"),
        }
    }

}

#[derive(Debug)]
struct Frame {
    time: i64, 
    keys: u32, 
}

#[derive(Debug)]
struct Replay {
    mode: u8,
    version: i32,
    beatmap_md5: String,
    player: String,
    n300: u16,
    n100: u16,
    n50: u16,
    n_geki: u16, 
    n_katu: u16, 
    n_miss: u16,
    score: i32,
    max_combo: u16,
    mods: i32,
    frames: Vec<Frame>,
}

fn parse_replay(bytes: &[u8]) -> Result<Replay> {
    let mut r = Reader::new(bytes);
    let mode = r.u8()?;
    let version = r.i32()?;
    let beatmap_md5 = r.string()?;
    let player = r.string()?;
    let _replay_md5 = r.string()?;
    let n300 = r.u16()?;
    let n100 = r.u16()?;
    let n50 = r.u16()?;
    let n_geki = r.u16()?;
    let n_katu = r.u16()?;
    let n_miss = r.u16()?;
    let score = r.i32()?;
    let max_combo = r.u16()?;
    let _perfect = r.u8()?;
    let mods = r.i32()?;
    let _life_bar = r.string()?;
    r.take(8)?; 
    let len = r.i32()?;
    let compressed = r.take(len as usize)?;

    let mut out = Vec::new();
    lzma_rs::lzma_decompress(&mut Cursor::new(compressed), &mut out)
        .map_err(|e| anyhow!("lzma error: {e:?}"))?;
    let text = String::from_utf8_lossy(&out);
    for f in text.split(',').take(5) {
        println!("raw: {f}");
    }   

    let mut frames = Vec::new();
    let mut t: i64 = 0;
    for part in text.split(',') {
    let mut f = part.split('|');
    let (Some(w), Some(x), Some(y), Some(_z)) = (f.next(), f.next(), f.next(), f.next())
    else {
        continue;
    };
    let w: i64 = w.parse()?;
    if w == -12345 {
        continue; 
    }
    t += w; 
    let y: f32 = y.parse()?;
    if y == -500.0 {
        continue; 
    }
    let x: f32 = x.parse()?;
    frames.push(Frame { time: t, keys: x as u32 });
}

    Ok(Replay {
        mode, version, beatmap_md5, player,
        n300, n100, n50, n_geki, n_katu, n_miss,
        score, max_combo, mods, frames,
    })
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let (Some(osr), Some(osz)) = (args.next(), args.next()) else {
        bail!("usage: omrdr <replay.osr> <map.osz> [out.mp4]");
    };
    let out = args.next().unwrap_or_else(|| "out.mp4".to_string());

    let replay = parse_replay(&fs::read(&osr)?)?;

    println!("player:   {}", replay.player);
    println!("mode:     {} (3 = mania)", replay.mode);
    println!("version:  {}", replay.version);
    println!("map md5:  {}", replay.beatmap_md5);
    println!("score:    {}   max combo: {}", replay.score, replay.max_combo);
    println!(
        "hits:     geki {} | 300 {} | katu {} | 100 {} | 50 {} | miss {}",
        replay.n_geki, replay.n300, replay.n_katu, replay.n100, replay.n50, replay.n_miss
    );
    println!("mods:     {}", replay.mods);
    println!("frames:   {}", replay.frames.len());

    let (bm, file) = beatmap::load_from_osz(Path::new(&osz), &replay.beatmap_md5)?;

    let notes = bm.objects.iter().filter(|o| o.end_time.is_none()).count();
    let holds = bm.objects.len() - notes;
    let replay_total = replay.n_geki as usize
        + replay.n300 as usize
        + replay.n_katu as usize
        + replay.n100 as usize
        + replay.n50 as usize
        + replay.n_miss as usize;

    println!("\nbeatmap:  {} [{}]  ({file})", bm.title, bm.version);
    println!("mode: {}   keys: {}   OD: {}", bm.mode, bm.keys, bm.od);
    println!("timing points: {}", bm.timing.len());
    println!("notes: {notes}   holds: {holds}");
    println!(
        "judgements in replay: {replay_total}   (notes+holds = {}, notes+2*holds = {})",
        notes + holds,
        notes + 2 * holds
    );
        let s = sim::simulate(&bm, &replay.frames);
    let row = |name: &str, c: &sim::Counts| {
        println!("{name:<10}{:>6} {:>5} {:>5} {:>5} {:>5} {:>5}",
            c.geki, c.n300, c.katu, c.n100, c.n50, c.miss);
    };
    println!("\n           geki   300  katu   100    50  miss");
    println!("replay:  {:>6} {:>5} {:>5} {:>5} {:>5} {:>5}",
        replay.n_geki, replay.n300, replay.n_katu, replay.n100, replay.n50, replay.n_miss);
    row("sim taps:", &s.tap);
    row("sim holds:", &s.hold);

    // combo check: does a 50 break combo in this ruleset? Use whichever fits the replay better.
    let plain = sim::stats(&s.events, false);
    let strict = sim::stats(&s.events, true);
    let max_of = |st: &sim::Stats| *st.combo.iter().max().unwrap_or(&0) as i64;
    println!(
        "\nmax combo: replay {} | sim {} | sim if 50s break combo {}",
        replay.max_combo, max_of(&plain), max_of(&strict)
    );
    let target = replay.max_combo as i64;
    let stats = if (max_of(&plain) - target).abs() <= (max_of(&strict) - target).abs() { plain } else { strict };

    // DT/NC speed the song up, HT slows it down
    let rate = if replay.mods & (64 | 512) != 0 { 1.5 } else if replay.mods & 256 != 0 { 0.75 } else { 1.0 };

    let ext = Path::new(&bm.audio_file).extension().and_then(|e| e.to_str()).unwrap_or("mp3");
    let audio_path = env::temp_dir().join(format!("omrdr_audio.{ext}"));
    beatmap::extract_audio(Path::new(&osz), &bm.audio_file, &audio_path)?;

    render::render(&bm, &replay.frames, &s.events, &stats, Some(&audio_path), rate, Path::new(&out))?;
    Ok(())
}