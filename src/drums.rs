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
        mpsc::{self, Receiver, Sender},
    },
    time::Duration,
};

use rodio::{
    ChannelCount, Decoder, SampleRate, Source,
    source::{SeekError, UniformSourceIterator},
};
use rosu_map::section::hit_objects::{
    HitObjectKind,
    hit_samples::{HitSampleInfo, HitSampleInfoName, SampleBank},
};

use crate::library::Song;

// ---------- Built-in samples ----------

macro_rules! samples {
    ($($name:literal)*) => {
        [$(($name, include_bytes!(concat!("../assets/", $name, ".flac")) as &[u8])),*]
    };
}

/// osu! classic skin samples, used when a beatmap doesn't ship its own.
const SAMPLES: [(&str, &[u8]); 15] = samples!(
    "normal-hitnormal" "normal-hitwhistle" "normal-hitfinish" "normal-hitclap" "normal-slidertick"
    "soft-hitnormal" "soft-hitwhistle" "soft-hitfinish" "soft-hitclap" "soft-slidertick"
    "drum-hitnormal" "drum-hitwhistle" "drum-hitfinish" "drum-hitclap" "drum-slidertick"
);

// ---------- Chart parsing ----------

/// One sound to play.
struct Hit {
    /// Milliseconds from the start of the song.
    time: f64,
    /// Beatmap file to try first (extension optional); empty means built-in only.
    file: String,
    /// Built-in sample, used when `file` is empty or missing, or when osu!'s own sounds are wanted.
    default: String,
    volume: f32,
}

/// Builds a hit for e.g. `soft-hitclap`. Custom index 0 uses the built-in sample only,
/// 1 looks for `soft-hitclap.wav` in the beatmap, N for `soft-hitclapN.wav`.
fn sample(time: f64, bank: SampleBank, sound: &str, index: i32, volume: i32) -> Hit {
    let default = format!("{bank}-{sound}");
    let file = match index {
        0 => String::new(),
        1 => default.clone(),
        i => format!("{default}{i}"),
    };
    Hit { time, file, default, volume: volume as f32 / 100. }
}

/// The hits of an object or slider node, whose samples have their bank, index and volume resolved already.
/// Like osu!stable (unlike lazer), a custom file plays instead of all the other sounds, additions included;
/// osu!'s hitnormal stands in when the file is missing.
fn hits_at(time: f64, samples: &[HitSampleInfo]) -> impl Iterator<Item = Hit> {
    let custom = samples.iter().find(|s| matches!(s.name, HitSampleInfoName::File(_)));
    custom.map_or(samples, std::slice::from_ref).iter().map(move |s| match &s.name {
        HitSampleInfoName::Default(sound) => sample(time, s.bank, sound.to_lowercase_str(), s.custom_sample_bank, s.volume),
        HitSampleInfoName::File(file) => Hit { time, file: file.clone(), default: "normal-hitnormal".into(), volume: s.volume as f32 / 100. },
    })
}

/// Parses a .osu file into the hitsounds an Auto play would trigger. rosu-map decodes it like
/// osu!lazer does, including the sample settings each object takes from the timing points.
fn chart(path: &Path) -> Vec<Hit> {
    let Ok(mut map) = rosu_map::Beatmap::from_path(path) else { return vec![] };
    let (points, tick_rate) = (&map.control_points, map.slider_tick_rate);
    let mut hits = vec![];
    for o in &mut map.hit_objects {
        let time = o.start_time;
        match &mut o.kind {
            // A hit at the head, every repeat and the tail, plus ticks along the body
            HitObjectKind::Slider(slider) => {
                let span = slider.duration() / slider.span_count() as f64;
                for (e, samples) in slider.node_samples.iter().enumerate() {
                    hits.extend(hits_at(time + e as f64 * span, samples));
                }

                // Ticks every beat / tick rate within each span (mirrored on reverse spans), skipping any within
                // 10 ms of the span's end; in the slider's bank, with the index and volume of the timing point there
                let interval = points.timing_point_at(time).map_or(1000., |p| p.beat_len) / tick_rate;
                let bank = o.samples.first().map_or(SampleBank::Normal, |s| s.bank);
                for e in 0..slider.span_count() {
                    let mut k = interval;
                    while interval > 1. && k < span - 10. {
                        let t = time + e as f64 * span + if e % 2 == 0 { k } else { span - k };
                        let p = points.sample_point_at(t + 5.).cloned().unwrap_or_default();
                        hits.push(sample(t, bank, "slidertick", p.custom_sample_bank, p.sample_volume));
                        k += interval;
                    }
                }
            }
            // Spinners sound once, at their end
            HitObjectKind::Spinner(spinner) => hits.extend(hits_at(time + spinner.duration, &o.samples)),
            _ => hits.extend(hits_at(time, &o.samples)),
        }
    }
    hits
}

// ---------- Sample loading ----------

/// Decodes a sample and converts it to the music's channel count and sample rate. The beatmap's
/// file is preferred unless `builtin` asks for osu!'s own sounds; a beatmap file that exists but won't
/// decode (often an intentionally empty .wav) stays silent.
fn load(files: &HashMap<String, PathBuf>, file: &str, default: &str, builtin: bool, channels: ChannelCount, rate: SampleRate) -> Option<Arc<[f32]>> {
    let file = if builtin { String::new() } else { file.to_lowercase() };
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

/// Hitsounds ready to mix: (start frame, samples in the music's format, volume), sorted by frame.
pub type Hits = Vec<(u64, Arc<[f32]>, f32)>;

/// Decodes the hitsounds of one difficulty (an index into `song.charts`) for music in the given format,
/// with the beatmap's own samples or, with `builtin`, osu!'s.
pub fn hits(song: &Song, chart_index: usize, builtin: bool, channels: ChannelCount, rate: SampleRate) -> Hits {
    let mut cache = HashMap::new();
    let mut hits: Hits = chart(&song.charts[chart_index].path)
        .into_iter()
        .filter_map(|h| {
            let samples = cache
                .entry((h.file, h.default))
                .or_insert_with_key(|(file, default)| load(&song.files, file, default, builtin, channels, rate))
                .clone()?;
            let frame = (h.time.max(0.0) * rate.get() as f64 / 1000.0) as u64;
            Some((frame, samples, h.volume * 0.7))
        })
        .collect();
    hits.sort_by_key(|h| h.0);
    hits
}

/// Opens a song for playback with the hitsounds of one difficulty (an index into `song.charts`).
/// Hits are always prepared so drum mode can be toggled mid-song via `on`; the returned sender
/// hands the playing source other hitsounds (see [`hits`]), e.g. of another difficulty.
pub fn open(song: &Song, chart_index: usize, builtin: bool, on: Arc<AtomicBool>) -> Option<(Drums<Decoder<BufReader<File>>>, Sender<Hits>)> {
    let music = Decoder::try_from(File::open(&song.audio).ok()?).ok()?;
    let (channels, rate) = (music.channels(), music.sample_rate());
    let (swap, swaps) = mpsc::channel();
    let hits = hits(song, chart_index, builtin, channels, rate);
    Some((Drums { src: music, hits, swaps, next: 0, frame: 0, channel: 0, voices: vec![], on }, swap))
}

// ---------- Mixing source ----------

/// Wraps the music source and adds hitsounds into it at exact sample frames.
pub struct Drums<S> {
    src: S,
    hits: Hits,
    /// Replacement hitsounds, when another difficulty is picked mid-song
    swaps: Receiver<Hits>,
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
            // Another difficulty was picked: carry on with its hitsounds from here
            if let Ok(hits) = self.swaps.try_recv() {
                self.next = hits.partition_point(|h| h.0 < self.frame);
                self.hits = hits;
            }
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
