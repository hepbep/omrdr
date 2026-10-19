use crate::{beatmap::{Beatmap, HitObject}, replay::Frame};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Grade { Perfect, Great, Good, Ok, Meh, Miss }

const GRADES: [Grade; 6] = [
    Grade::Perfect, Grade::Great, Grade::Good, Grade::Ok, Grade::Meh, Grade::Miss,
];

#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum Combine { Worst, HeadOnly, TailOnly, Average }

#[derive(Clone, Debug)]
pub struct Params {
    pub perfect: f64,
    pub tail_scale: f64,
    pub combine: Combine,
    pub miss_consumes: bool,
    pub late_consumes: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            perfect: 16.0,
            tail_scale: 2.5,
            combine: Combine::Worst,
            miss_consumes: false,
            late_consumes: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Counts {
    pub geki: u32, pub n300: u32, pub katu: u32,
    pub n100: u32, pub n50: u32, pub miss: u32,
}

impl Counts {
    fn add(&mut self, g: Grade) {
        match g {
            Grade::Perfect => self.geki += 1,
            Grade::Great => self.n300 += 1,
            Grade::Good => self.katu += 1,
            Grade::Ok => self.n100 += 1,
            Grade::Meh => self.n50 += 1,
            Grade::Miss => self.miss += 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HitEvent {
    pub time: f64,
    pub column: usize,
    pub note_time: i32,
    pub grade: Grade,
    pub offset: Option<f64>, // press error in ms, taps only
    pub is_hold: bool,
}

#[derive(Debug, Default)]
pub struct Split {
    pub tap: Counts,
    pub hold: Counts,
    pub events: Vec<HitEvent>,
}

fn put(s: &mut Split, col: usize, o: &HitObject, time: f64, g: Grade, offset: Option<f64>) {
    if o.end_time.is_some() { s.hold.add(g) } else { s.tap.add(g) }
    s.events.push(HitEvent {
        time, column: col, note_time: o.time, grade: g, offset,
        is_hold: o.end_time.is_some(),
    });
}

struct Windows { perfect: f64, great: f64, good: f64, ok: f64, meh: f64, miss: f64 }

fn windows(od: f32, perfect: f64) -> Windows {
    let od = od as f64;
    Windows {
        perfect,
        great: 64.0 - 3.0 * od,
        good: 97.0 - 3.0 * od,
        ok: 127.0 - 3.0 * od,
        meh: 151.0 - 3.0 * od,
        miss: 188.0 - 3.0 * od,
    }
}

fn grade(offset: f64, w: &Windows, scale: f64) -> Grade {
    let a = offset.abs();
    if a <= w.perfect * scale { Grade::Perfect }
    else if a <= w.great * scale { Grade::Great }
    else if a <= w.good * scale { Grade::Good }
    else if a <= w.ok * scale { Grade::Ok }
    else if a <= w.meh * scale { Grade::Meh }
    else { Grade::Miss }
}

fn combine(h: Grade, t: Grade, c: Combine) -> Grade {
    match c {
        Combine::Worst => h.max(t),
        Combine::HeadOnly => h,
        Combine::TailOnly => t,
        Combine::Average => GRADES[((h as usize) + (t as usize)) / 2],
    }
}

pub fn simulate(bm: &Beatmap, frames: &[Frame]) -> Split {
    simulate_with(bm, frames, &Params::default())
}

pub fn simulate_with(bm: &Beatmap, frames: &[Frame], p: &Params) -> Split {
    let w = windows(bm.od, p.perfect);
    let early_limit = if p.miss_consumes { w.miss } else { w.meh };
    let late_limit = if p.late_consumes { w.miss } else { w.meh };
    let mut split = Split::default();

    for col in 0..bm.keys {
        let mut edges: Vec<(f64, bool)> = Vec::new();
        let mut held = false;
        for f in frames {
            let now = (f.keys >> col) & 1 == 1;
            if now != held {
                edges.push((f.time as f64, now));
                held = now;
            }
        }

        let objs: Vec<&HitObject> = bm.objects.iter().filter(|o| o.column == col).collect();
        let mut next = 0;
        let mut active: Option<(usize, Grade)> = None;

        for &(t, pressed) in &edges {
            if let Some((i, head)) = active {
                let end = objs[i].end_time.unwrap() as f64;
                if t > end + w.miss * p.tail_scale {
                    put(&mut split, col, objs[i], end, combine(head, Grade::Perfect, p.combine), None);
                    active = None;
                }
            }
            while active.is_none() && next < objs.len() && t > objs[next].time as f64 + late_limit {
                put(&mut split, col, objs[next], objs[next].time as f64, Grade::Miss, None);
                next += 1;
            }

            if pressed {
                if active.is_none() && next < objs.len() {
                    let o = objs[next];
                    let off = t - o.time as f64;
                    if off >= -early_limit {
                        let g = grade(off, &w, 1.0);
                        if o.end_time.is_some() && g != Grade::Miss {
                            active = Some((next, g));
                        } else {
                            let offset = if o.end_time.is_none() { Some(off) } else { None };
                            put(&mut split, col, o, t, g, offset);
                        }
                        next += 1;
                    }
                }
            } else if let Some((i, head)) = active.take() {
                let end = objs[i].end_time.unwrap() as f64;
                let tail = grade(t - end, &w, p.tail_scale);
                put(&mut split, col, objs[i], t, combine(head, tail, p.combine), None);
            }
        }

        if let Some((i, head)) = active {
            let end = objs[i].end_time.unwrap() as f64;
            put(&mut split, col, objs[i], end, combine(head, Grade::Perfect, p.combine), None);
        }
        while next < objs.len() {
            put(&mut split, col, objs[next], objs[next].time as f64, Grade::Miss, None);
            next += 1;
        }
    }

    split.events.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap());
    split
}

fn weight(g: Grade) -> f32 {
    match g {
        Grade::Perfect | Grade::Great => 300.0,
        Grade::Good => 200.0,
        Grade::Ok => 100.0,
        Grade::Meh => 50.0,
        Grade::Miss => 0.0,
    }
}

pub struct Stats {
    pub combo: Vec<u32>,
    pub acc: Vec<f32>,
    pub counts: Vec<[u32; 6]>, 
}

pub fn stats(events: &[HitEvent], meh_breaks: bool) -> Stats {
    let mut combo = vec![0u32];
    let mut acc = vec![1.0f32];
    let mut counts = vec![[0u32; 6]];
    let (mut c, mut sum) = (0u32, 0.0f32);
    let mut tally = [0u32; 6];
    for (i, e) in events.iter().enumerate() {
        let step = if e.is_hold { 2 } else { 1 };
        c = if e.grade == Grade::Miss || (meh_breaks && e.grade == Grade::Meh) { 0 } else { c + step };
        sum += weight(e.grade);
        tally[e.grade as usize] += 1;
        combo.push(c);
        acc.push(sum / (300.0 * (i as f32 + 1.0)));
        counts.push(tally);
    }
    Stats { combo, acc, counts }
}