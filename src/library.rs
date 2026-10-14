//! Scans the osu!stable and osu!lazer libraries into a list of songs.
//!
//! Both are normalised to "beatmap set = file name → real path": for stable that's simply
//! a folder under Songs; lazer stores files by hash, so the mapping comes from client.realm.

use std::{
    cmp::Reverse,
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};

use rayon::prelude::*;

use crate::realm::Realm;

/// Lowercase file name → real path
type Files = HashMap<String, PathBuf>;

#[derive(Clone)]
pub struct Song {
    pub title: String,
    pub artist: String,
    pub creator: String,
    /// Lowercase title / artist / mapper / source / tags, for searching and de-duplication
    pub key: String,
    pub audio: PathBuf,
    pub cover: Option<PathBuf>,
    /// Every difficulty that uses this audio file
    pub charts: Vec<PathBuf>,
    /// All files of the beatmap set, where drum mode looks for custom samples
    pub files: Arc<Files>,
}

// ---------- Manually added library folders ----------

fn config() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("osu-music.txt")
}

pub fn add_folder(path: &Path) {
    let list = fs::read_to_string(config()).unwrap_or_default();
    fs::write(config(), format!("{list}{}\n", path.display())).ok();
}

// ---------- Entry point ----------

pub fn scan() -> Vec<Song> {
    // Default locations: stable on Windows, lazer on every platform, stable via osu-winello on Linux
    let roots = [
        dirs::data_local_dir().map(|d| d.join("osu!")),
        dirs::data_dir().map(|d| d.join("osu")),
        dirs::home_dir().map(|d| d.join(".local/share/osu-wine/osu!")),
    ];
    let extra = fs::read_to_string(config()).unwrap_or_default();
    let sets: Vec<Files> = roots
        .into_iter()
        .flatten()
        .chain(extra.lines().map(PathBuf::from))
        .flat_map(|root| sets_in(&root))
        .collect();

    let mut songs: Vec<Song> = sets.into_par_iter().flat_map_iter(songs_of).collect();

    // Duplicates come from maps present in both stable and lazer, and from rate edits that reuse
    // the set's metadata with a sped-up audio file. Keep the largest file: the original-speed version.
    songs.sort_by_cached_key(|s| (s.key.clone(), Reverse(fs::metadata(&s.audio).map_or(0, |m| m.len())), s.audio.clone()));
    songs.dedup_by(|a, b| a.key == b.key);

    songs.sort_by_cached_key(|s| (s.artist.to_lowercase(), s.title.to_lowercase()));
    songs
}

/// Reads a `key = value` file (lazer's storage.ini, stable's osu!.*.cfg).
fn ini(path: &Path, key: &str) -> Option<String> {
    fs::read_to_string(path).ok()?.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

/// Detects whether `root` is a lazer or stable install and returns its beatmap sets.
fn sets_in(root: &Path) -> Vec<Files> {
    // lazer can move its data elsewhere; storage.ini then points to the new location
    let root = ini(&root.join("storage.ini"), "FullPath").map_or(root.to_path_buf(), PathBuf::from);
    if root.join("client.realm").is_file() {
        // The hand-written Realm reader panics (out of bounds) on layouts it doesn't know; contain that
        return std::panic::catch_unwind(|| lazer(&root)).ok().flatten().unwrap_or_default();
    }
    stable(&root)
}

// ---------- osu!stable ----------

fn stable(root: &Path) -> Vec<Files> {
    // The Songs folder can be moved in osu!.<username>.cfg; if `root` is the Songs folder itself, use it
    let cfg = fs::read_dir(root).into_iter().flatten().flatten().find(|e| {
        let name = e.file_name().to_string_lossy().to_lowercase();
        name.starts_with("osu!.") && name.ends_with(".cfg")
    });
    let songs = cfg
        .and_then(|e| ini(&e.path(), "BeatmapDirectory"))
        .map_or(root.join("Songs"), |d| root.join(d));
    let songs = if songs.is_dir() { songs } else { root.to_path_buf() };

    fs::read_dir(songs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|set| {
            let files = fs::read_dir(set.path()).ok()?.flatten();
            Some(files.map(|f| (f.file_name().to_string_lossy().to_lowercase(), f.path())).collect())
        })
        .collect()
}

// ---------- osu!lazer ----------

/// Schema: BeatmapSet.Files → RealmNamedFileUsage { Filename, File → class_File.Hash },
/// with the actual file at files/<hash[0]>/<hash[0..2]>/<hash>.
fn lazer(root: &Path) -> Option<Vec<Files>> {
    let db = Realm::open(&root.join("client.realm")).ok()?;

    // 1. File row key → hash
    let mut hashes = HashMap::new();
    let (t, c) = db.table("class_File")?;
    db.rows(t, 0, &mut |key, leaf, i| {
        hashes.extend(db.string(db.get(leaf, c["Hash"]), i).map(|h| (key, h)));
    });

    // 2. File usage row key → (file name, real path); a single link stores "row key + 1", 0 = null
    let mut usages = HashMap::new();
    let (t, c) = db.table("class_RealmNamedFileUsage")?;
    db.rows(t, 0, &mut |key, leaf, i| {
        let hash = db.get(db.get(leaf, c["File"]), i).checked_sub(1).and_then(|k| hashes.get(&k));
        if let (Some(h), Some(name)) = (hash, db.string(db.get(leaf, c["Filename"]), i)) {
            let path = root.join("files").join(&h[..1]).join(&h[..2]).join(h);
            usages.insert(key, (name.to_lowercase(), path));
        }
    });

    // 3. Expand the file list of every set that isn't pending deletion
    let mut sets = vec![];
    let (t, c) = db.table("class_BeatmapSet")?;
    db.rows(t, 0, &mut |_, leaf, i| {
        let list = db.get(db.get(leaf, c["Files"]), i);
        if db.get(db.get(leaf, c["DeletePending"]), i) == 0 && list != 0 {
            let mut keys = vec![];
            db.list(list, &mut keys);
            sets.push(keys.iter().filter_map(|k| usages.get(k).cloned()).collect());
        }
    });
    Some(sets)
}

// ---------- Beatmap set → songs ----------

/// Reads only the head of a .osu file (up to [TimingPoints]): its `key: value` fields and background file.
fn header(path: &Path) -> Option<(HashMap<String, String>, Option<String>)> {
    let (mut kv, mut bg, mut events) = (HashMap::new(), None, false);
    for line in BufReader::new(fs::File::open(path).ok()?).split(b'\n') {
        let line = String::from_utf8_lossy(&line.ok()?).trim().to_string();
        if line.starts_with('[') {
            if line == "[TimingPoints]" || line == "[HitObjects]" {
                break;
            }
            events = line == "[Events]";
        } else if events {
            // Background event: 0,0,"bg.jpg",x,y
            if bg.is_none() && line.starts_with("0,0,") {
                bg = line.split(',').nth(2).map(|s| s.trim_matches('"').to_lowercase());
            }
        } else if let Some((k, v)) = line.split_once(':') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Some((kv, bg))
}

/// Groups a set's difficulties by audio file: usually one song per set, occasionally several.
fn songs_of(files: Files) -> Vec<Song> {
    let files = Arc::new(files);
    let mut songs: Vec<Song> = vec![];

    // Sorted so the metadata always comes from the same difficulty
    let mut charts: Vec<_> = files.iter().filter(|(name, _)| name.ends_with(".osu")).map(|(_, path)| path).collect();
    charts.sort();

    for chart in charts {
        let Some((kv, bg)) = header(chart) else { continue };
        let Some(audio) = kv.get("AudioFilename").and_then(|a| files.get(&a.to_lowercase())) else {
            continue;
        };
        if let Some(song) = songs.iter_mut().find(|s| &s.audio == audio) {
            song.charts.push(chart.clone());
            continue;
        }

        let get = |k: &str| kv.get(k).cloned().unwrap_or_default();
        // Prefer the original-script title / artist, falling back to the romanised one
        let unicode = |k: &str, fallback: &str| Some(get(k)).filter(|v| !v.is_empty()).unwrap_or_else(|| get(fallback));
        let key = ["Title", "TitleUnicode", "Artist", "ArtistUnicode", "Creator", "Source", "Tags"].map(get);
        songs.push(Song {
            title: unicode("TitleUnicode", "Title"),
            artist: unicode("ArtistUnicode", "Artist"),
            creator: get("Creator"),
            key: key.join(" ").to_lowercase(),
            audio: audio.clone(),
            cover: bg.and_then(|b| files.get(&b).cloned()),
            charts: vec![chart.clone()],
            files: files.clone(),
        });
    }
    songs
}
