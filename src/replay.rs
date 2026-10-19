use std::io::Cursor;
use anyhow::{anyhow, bail, Result};

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
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

#[derive(Debug, Clone)]
pub struct Frame {
    pub time: i64,
    pub keys: u32,
}

#[derive(Debug)]
pub struct Replay {
    pub mode: u8,
    pub version: i32,
    pub beatmap_md5: String,
    pub player: String,
    pub n300: u16,
    pub n100: u16,
    pub n50: u16,
    pub n_geki: u16,
    pub n_katu: u16,
    pub n_miss: u16,
    pub score: i32,
    pub max_combo: u16,
    pub mods: i32,
    pub frames: Vec<Frame>,
}

pub fn parse_replay(bytes: &[u8]) -> Result<Replay> {
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
    if len < 0 {
        bail!("corrupt replay data length");
    }
    let compressed = r.take(len as usize)?;

    let mut out = Vec::new();
    lzma_rs::lzma_decompress(&mut Cursor::new(compressed), &mut out)
        .map_err(|e| anyhow!("lzma error: {e:?}"))?;
    let text = String::from_utf8_lossy(&out);

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