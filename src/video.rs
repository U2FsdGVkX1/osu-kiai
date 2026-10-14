//! Background video playback. FFmpeg decodes the beatmap video into raw BGRA frames
//! through a pipe, and the UI shows whichever frame is due at the music's position.

use std::{
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError, sync_channel},
    },
    thread,
};

use gpui_kit::RenderImage;
use image::{Frame, RgbaImage};

// It sits behind a scrim, so a modest resolution is plenty and keeps decoding cheap
const WIDTH: u32 = 960;
const HEIGHT: u32 = 540;
const FPS: f64 = 30.;

fn ffmpeg() -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.stdin(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        // Don't flash a console window from a GUI app
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Whether an `ffmpeg` executable can be found.
pub fn available() -> bool {
    ffmpeg().arg("-version").stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// How to install FFmpeg on this platform.
pub const INSTALL_COMMAND: &str = if cfg!(windows) {
    "winget install Gyan.FFmpeg"
} else if cfg!(target_os = "macos") {
    "brew install ffmpeg"
} else {
    "sudo apt install ffmpeg"
};

pub const DOWNLOAD_URL: &str = "https://ffmpeg.org/download.html";

pub struct Video {
    child: Child,
    /// Decoded frames with the music time (ms) they belong to
    frames: Receiver<(f64, Arc<RenderImage>)>,
    /// A frame read from the queue that isn't due yet
    next: Option<(f64, Arc<RenderImage>)>,
    /// The frame to show now
    pub frame: Option<Arc<RenderImage>>,
    /// Music time (ms) of the newest frame shown, or of where decoding started
    time: f64,
    /// FFmpeg reached the end of the video
    ended: bool,
}

impl Video {
    /// Starts decoding so that frames line up with music time `from` (ms).
    /// `offset` is the music time the video starts at.
    pub fn open(path: &Path, offset: f64, from: f64) -> Option<Self> {
        let start = (from - offset).max(0.);
        // Fill a fixed 16:9 frame (cropping as needed) so the byte size of every frame is known
        let filter = format!("scale={WIDTH}:{HEIGHT}:force_original_aspect_ratio=increase,crop={WIDTH}:{HEIGHT}");
        let mut child = ffmpeg()
            .args(["-ss", &format!("{:.3}", start / 1000.), "-i"])
            .arg(path)
            .args(["-an", "-vf", &filter, "-r", &FPS.to_string(), "-pix_fmt", "bgra", "-f", "rawvideo", "-"])
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;

        // A short queue: once it's full FFmpeg blocks on the pipe, so decoding never runs far ahead
        let (tx, frames) = sync_channel(3);
        thread::spawn(move || {
            for n in 0.. {
                let mut buf = vec![0; (WIDTH * HEIGHT * 4) as usize];
                if stdout.read_exact(&mut buf).is_err() {
                    break;
                }
                // The bytes are BGRA, which is exactly what RenderImage expects
                let image = RgbaImage::from_raw(WIDTH, HEIGHT, buf).unwrap();
                let time = offset + start + n as f64 * 1000. / FPS;
                if tx.send((time, Arc::new(RenderImage::new([Frame::new(image)])))).is_err() {
                    break;
                }
            }
        });

        Some(Self { child, frames, next: None, frame: None, time: offset + start, ended: false })
    }

    /// Moves to the newest frame due at music time `pos` (ms) and returns the frames it replaced,
    /// so the caller can free their GPU textures.
    pub fn advance(&mut self, pos: f64) -> Vec<Arc<RenderImage>> {
        let mut replaced = vec![];
        loop {
            let next = self.next.take().or_else(|| match self.frames.try_recv() {
                Ok(next) => Some(next),
                Err(e) => {
                    self.ended |= e == TryRecvError::Disconnected;
                    None
                }
            });
            let Some((time, frame)) = next else { break };
            if time > pos {
                self.next = Some((time, frame));
                break;
            }
            self.time = time;
            replaced.extend(self.frame.replace(frame));
        }
        replaced
    }

    /// Whether decoding has fallen well behind the music, e.g. after the window was hidden for a while
    /// (nothing is drawn then, so the queue stays full and FFmpeg waits).
    pub fn lagging(&self, pos: f64) -> bool {
        !self.ended && self.next.is_none() && pos - self.time > 1000.
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}
