//! Drum mode: turns a chart's hit objects into the hitsounds an osu! Auto play would trigger,
//! and mixes them sample-accurately into the music stream.

use std::{
    borrow::Cow,
    collections::HashMap,
    fs::File,
    io::{BufReader, Cursor},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rodio::{
    ChannelCount, Decoder, SampleRate, Source,
    source::{SeekError, UniformSourceIterator},
};

use crate::library::Song;

// ---------- Built-in samples ----------

macro_rules! samples {
    ($($name:literal)*) => {
        [$(($name, include_bytes!(concat!("../assets/", $name, ".wav")) as &[u8])),*]
    };
}

/// osu! classic skin samples, used when a beatmap doesn't ship its own.
const SAMPLES: [(&str, &[u8]); 15] = samples!(
    "normal-hitnormal" "normal-hitwhistle" "normal-hitfinish" "normal-hitclap" "normal-slidertick"
    "soft-hitnormal" "soft-hitwhistle" "soft-hitfinish" "soft-hitclap" "soft-slidertick"
    "drum-hitnormal" "drum-hitwhistle" "drum-hitfinish" "drum-hitclap" "drum-slidertick"
);

/// Sample banks in .osu numbering: 1 = normal, 2 = soft, 3 = drum (0 = inherit).
const BANKS: [&str; 3] = ["normal", "soft", "drum"];

// ---------- Chart parsing ----------

/// One sound to play.
struct Hit {
    /// Milliseconds from the start of the song.
    time: f64,
    /// Beatmap file to try first (extension optional); empty means built-in only.
    file: String,
    /// Built-in sample used when `file` isn't in the beatmap.
    default: String,
    volume: f32,
}

/// A timing point with inherited values already resolved.
struct Point {
    time: f64,
    /// Beat length of the governing uninherited (red) line, in ms.
    beat: f64,
    /// Slider velocity multiplier from an inherited (green) line; 1.0 on red lines.
    sv: f64,
    bank: usize,
    index: u32,
    volume: f64,
}

/// The `normalSet:additionSet:index:volume:filename` part of a hit object.
/// Zero values mean "inherit from the timing point".
#[derive(Clone, Copy)]
struct HitSample<'a> {
    normal: usize,
    addition: usize,
    index: u32,
    volume: f64,
    file: &'a str,
}

impl<'a> HitSample<'a> {
    fn parse(s: Option<&'a str>) -> Self {
        let mut parts = s.unwrap_or_default().split(':');
        let mut num = || parts.next().and_then(|p| p.trim().parse::<f64>().ok()).unwrap_or(0.0);
        let (normal, addition, index, volume) = (num() as usize, num() as usize, num() as u32, num());
        Self { normal, addition, index, volume, file: parts.next().unwrap_or_default().trim() }
    }
}

/// Builds a hit for e.g. `soft-hitclap`. Index 0 uses the built-in sample only,
/// index 1 looks for `soft-hitclap.wav` in the beatmap, index N for `soft-hitclapN.wav`.
fn sample(time: f64, bank: usize, sound: &str, index: u32, volume: f32) -> Hit {
    let default = format!("{}-{sound}", BANKS[bank - 1]);
    let file = match index {
        0 => String::new(),
        1 => default.clone(),
        i => format!("{default}{i}"),
    };
    Hit { time, file, default, volume }
}

/// Everything needed to place hitsounds on the timeline.
struct Timing {
    points: Vec<Point>,
    /// `[General] SampleSet`, the last fallback when no bank is set anywhere else.
    default_bank: usize,
    /// `[Difficulty] SliderMultiplier`
    multiplier: f64,
    /// `[Difficulty] SliderTickRate`
    tick_rate: f64,
}

impl Timing {
    fn new(kv: &HashMap<&str, &str>, rows: &[Vec<&str>]) -> Option<Self> {
        let num = |k: &str, default: f64| kv.get(k).and_then(|v| v.parse().ok()).unwrap_or(default);
        let default_bank = match kv.get("SampleSet") {
            Some(&"Soft") => 2,
            Some(&"Drum") => 3,
            _ => 1,
        };

        // Row format: time,beatLength,meter,sampleSet,sampleIndex,volume,uninherited,effects
        let mut beat = 500.0;
        let points: Vec<Point> = rows
            .iter()
            .filter_map(|p| {
                let f = |i: usize| p.get(i).and_then(|s| s.parse::<f64>().ok());
                let (time, len) = (f(0)?, f(1)?);
                // Old formats lack the `uninherited` column; a negative beat length marks a green line
                let red = len > 0.0 && f(6) != Some(0.0);
                if red {
                    beat = len;
                }
                let sv = if red || len.is_nan() { 1.0 } else { (-100.0 / len).clamp(0.1, 10.0) };
                let (bank, index, volume) = (f(3).unwrap_or(0.0) as usize, f(4).unwrap_or(0.0) as u32, f(5).unwrap_or(100.0));
                Some(Point { time, beat, sv, bank, index, volume })
            })
            .collect();

        (!points.is_empty()).then(|| Self {
            points,
            default_bank,
            multiplier: num("SliderMultiplier", 1.4),
            tick_rate: num("SliderTickRate", 1.0),
        })
    }

    /// The timing point in effect at `t` (the first one if `t` precedes them all).
    fn at(&self, t: f64) -> &Point {
        &self.points[self.points.partition_point(|p| p.time <= t).saturating_sub(1)]
    }

    /// Resolves a bank where 0 means "inherit": object → timing point → beatmap default.
    fn bank(&self, bank: usize, p: &Point) -> usize {
        [bank, p.bank].into_iter().find(|b| (1..=3).contains(b)).unwrap_or(self.default_bank)
    }

    /// Sounds for a single hit: hitnormal always, plus whistle / finish / clap for each set bit.
    fn node(&self, hits: &mut Vec<Hit>, t: f64, sound: u32, s: HitSample) {
        // Like osu!, read sample settings slightly late so a timing point placed just after the note still applies
        let p = self.at(t + 5.0);
        let volume = (if s.volume > 0.0 { s.volume } else { p.volume } / 100.0) as f32;

        // A custom filename replaces all of the object's sounds
        if !s.file.is_empty() {
            return hits.push(Hit { time: t, file: s.file.to_string(), default: String::new(), volume });
        }

        let index = if s.index > 0 { s.index } else { p.index };
        let normal = self.bank(s.normal, p);
        let addition = if (1..=3).contains(&s.addition) { s.addition } else { normal };
        hits.push(sample(t, normal, "hitnormal", index, volume));
        for (bit, name) in [(2, "hitwhistle"), (4, "hitfinish"), (8, "hitclap")] {
            if sound & bit != 0 {
                hits.push(sample(t, addition, name, index, volume));
            }
        }
    }

    /// Slider row: x,y,time,type,hitSound,curve,slides,length,edgeSounds,edgeSets,hitSample.
    /// Plays a hit at the head, every repeat and the tail, plus ticks along the body.
    fn slider(&self, hits: &mut Vec<Hit>, o: &[&str], time: f64, sound: u32) {
        let f = |i: usize| o.get(i).and_then(|s| s.trim().parse::<f64>().ok());
        let s = HitSample::parse(o.get(10).copied());
        let p = self.at(time);
        let span = f(7).unwrap_or(0.0) / (self.multiplier * 100.0 * p.sv) * p.beat;
        let slides = f(6).unwrap_or(1.0).max(1.0) as usize;

        // edgeSounds / edgeSets override the object's sound and banks per edge
        let split = |i: usize| o.get(i).map_or(vec![], |s| s.split('|').collect::<Vec<_>>());
        let (sounds, sets) = (split(8), split(9));
        for e in 0..=slides {
            let sound = sounds.get(e).and_then(|s| s.parse().ok()).unwrap_or(sound);
            let set = HitSample::parse(sets.get(e).copied());
            let pick = |a: usize, b: usize| if a > 0 { a } else { b };
            let edge = HitSample { normal: pick(set.normal, s.normal), addition: pick(set.addition, s.addition), ..s };
            self.node(hits, time + e as f64 * span, sound, edge);
        }

        // Ticks every beat / tick rate within each span (mirrored on reverse spans),
        // skipping any within 10 ms of the span's end
        let (interval, bank) = (p.beat / self.tick_rate, self.bank(s.normal, self.at(time + 5.0)));
        for e in 0..slides {
            let mut k = interval;
            while interval > 1.0 && k < span - 10.0 {
                let t = time + e as f64 * span + if e % 2 == 0 { k } else { span - k };
                let p = self.at(t + 5.0);
                hits.push(sample(t, bank, "slidertick", p.index, (p.volume / 100.0) as f32));
                k += interval;
            }
        }
    }
}

/// Parses a .osu file into the hitsounds an Auto play would trigger.
fn chart(path: &Path) -> Vec<Hit> {
    let text = String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default()).into_owned();

    // Split into `key: value` pairs, timing point rows and hit object rows
    let (mut section, mut kv, mut points, mut objects) = ("", HashMap::new(), vec![], vec![]);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
        } else if section == "[TimingPoints]" {
            points.push(line.split(',').map(str::trim).collect::<Vec<_>>());
        } else if section == "[HitObjects]" {
            objects.push(line.split(',').collect::<Vec<_>>());
        } else if let Some((k, v)) = line.split_once(':') {
            kv.insert(k.trim(), v.trim());
        }
    }

    let Some(timing) = Timing::new(&kv, &points) else { return vec![] };

    // Row format: x,y,time,type,hitSound,...; type bits: 1 circle, 2 slider, 8 spinner, 128 hold note
    let mut hits = vec![];
    for o in &objects {
        let f = |i: usize| o.get(i).and_then(|s| s.trim().parse::<f64>().ok());
        let (Some(time), Some(kind), Some(sound)) = (f(2), f(3), f(4)) else { continue };
        let (kind, sound) = (kind as u32, sound as u32);
        match kind {
            k if k & 2 != 0 => timing.slider(&mut hits, o, time, sound),
            // Spinners sound once, at their end time
            k if k & 8 != 0 => timing.node(&mut hits, f(5).unwrap_or(time), sound, HitSample::parse(o.get(6).copied())),
            // Hold notes put the end time in front of the hit sample: endTime:normalSet:...
            k if k & 128 != 0 => {
                let s = o.get(5).and_then(|s| s.split_once(':')).map(|s| s.1);
                timing.node(&mut hits, time, sound, HitSample::parse(s));
            }
            _ => timing.node(&mut hits, time, sound, HitSample::parse(o.get(5).copied())),
        }
    }
    hits
}

// ---------- Sample loading ----------

/// Decodes a sample and converts it to the music's channel count and sample rate.
/// A beatmap file that exists but won't decode (often an intentionally empty .wav) stays silent.
fn load(files: &HashMap<String, PathBuf>, file: &str, default: &str, channels: ChannelCount, rate: SampleRate) -> Option<Arc<[f32]>> {
    let file = file.to_lowercase();
    let custom = ["", ".wav", ".ogg", ".mp3"]
        .iter()
        .find_map(|ext| files.get(&format!("{file}{ext}")))
        .filter(|_| !file.is_empty());
    let bytes: Cow<'static, [u8]> = match custom {
        Some(path) => std::fs::read(path).ok()?.into(),
        None => SAMPLES.iter().find(|s| s.0 == default)?.1.into(),
    };

    let decoder = Decoder::try_from(Cursor::new(bytes)).ok()?;
    let samples: Arc<[f32]> = UniformSourceIterator::new(decoder, channels, rate).collect();
    (!samples.is_empty()).then_some(samples)
}

/// Opens a song for playback. Hits are always prepared so drum mode can be toggled mid-song via `on`.
pub fn open(song: &Song, on: Arc<AtomicBool>) -> Option<Drums<Decoder<BufReader<File>>>> {
    let music = Decoder::try_from(File::open(&song.audio).ok()?).ok()?;
    let (channels, rate) = (music.channels(), music.sample_rate());

    // Use the difficulty with the most hitsounds
    let hits = song.charts.iter().map(|c| chart(c)).max_by_key(Vec::len).unwrap_or_default();

    let mut cache = HashMap::new();
    let mut hits: Vec<_> = hits
        .into_iter()
        .filter_map(|h| {
            let samples = cache
                .entry((h.file, h.default))
                .or_insert_with_key(|(file, default)| load(&song.files, file, default, channels, rate))
                .clone()?;
            let frame = (h.time.max(0.0) * rate.get() as f64 / 1000.0) as u64;
            Some((frame, samples, h.volume * 0.7))
        })
        .collect();
    hits.sort_by_key(|h| h.0);

    Some(Drums { src: music, hits, next: 0, frame: 0, channel: 0, voices: vec![], on })
}

// ---------- Mixing source ----------

/// Wraps the music source and adds hitsounds into it at exact sample frames.
pub struct Drums<S> {
    src: S,
    /// (start frame, samples, volume), sorted by frame
    hits: Vec<(u64, Arc<[f32]>, f32)>,
    /// Index of the next hit to start
    next: usize,
    /// Current frame (one sample per channel)
    frame: u64,
    /// Channel of the next sample within the current frame
    channel: usize,
    /// Sounds currently playing: (samples, read position, volume)
    voices: Vec<(Arc<[f32]>, usize, f32)>,
    on: Arc<AtomicBool>,
}

impl<S: Source> Iterator for Drums<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let mut s = self.src.next()?;

        // At each frame boundary, start every hit that is due
        if self.channel == 0 {
            while let Some((t, samples, volume)) = self.hits.get(self.next)
                && *t <= self.frame
            {
                if self.on.load(Ordering::Relaxed) {
                    self.voices.push((samples.clone(), 0, *volume));
                }
                self.next += 1;
            }
            self.frame += 1;
        }
        self.channel = (self.channel + 1) % self.src.channels().get() as usize;

        // Samples share the music's channel layout, so they mix in step
        self.voices.retain_mut(|(samples, pos, volume)| {
            s += samples[*pos] * *volume;
            *pos += 1;
            *pos < samples.len()
        });
        Some(s)
    }
}

impl<S: Source> Source for Drums<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.src.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.src.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.src.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.src.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.src.try_seek(pos)?;
        self.frame = (pos.as_secs_f64() * self.sample_rate().get() as f64) as u64;
        self.next = self.hits.partition_point(|h| h.0 < self.frame);
        self.channel = 0;
        self.voices.clear();
        Ok(())
    }
}
