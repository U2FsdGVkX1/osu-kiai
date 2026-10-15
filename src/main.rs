//! osu! music: plays the songs in your osu!stable / osu!lazer library like a regular music player,
//! with an optional drum mode that adds the hitsounds an Auto play would make,
//! and a video mode that plays the beatmap's video behind the UI.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod drums;
mod library;
mod realm;
mod video;

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    time::{Duration, Instant},
};

use gpui_kit::{
    assets::{AllAssets, IconName},
    component::{
        ActiveTheme, Icon, IndexPath, Sizable, StyledExt, Theme, WindowExt,
        button::{Button, ButtonRounded, ButtonVariants},
        clipboard::Clipboard,
        dialog::{DialogClose, DialogFooter},
        h_flex,
        input::{Input, InputEvent, InputState},
        select::{Select, SelectEvent, SelectState},
        slider::{Slider, SliderEvent, SliderState},
        v_flex,
    },
    prelude::*,
    *,
};
use drums::Hits;
use library::{CATEGORIES, Song};
use rayon::prelude::*;
use rodio::{ChannelCount, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source};
use rust_i18n::t;
use video::Video;

rust_i18n::i18n!("locales", fallback = "en");

#[derive(Clone, Copy)]
enum Mode {
    Repeat,
    Order,
    Shuffle,
}

impl Mode {
    fn next(self) -> Self {
        match self {
            Mode::Repeat => Mode::Order,
            Mode::Order => Mode::Shuffle,
            Mode::Shuffle => Mode::Repeat,
        }
    }
}

struct MusicPlayer {
    // Library
    songs: Vec<Song>,
    /// Indices into `songs` that match the search, in display order
    shown: Vec<usize>,
    scanning: bool,

    // Playback
    current: Option<usize>,
    /// Previously played songs, popped by the "previous" button
    history: Vec<usize>,
    mode: Mode,
    /// Shared with the playing source so drum mode can be toggled mid-song
    drums: Arc<AtomicBool>,
    /// Drum mode uses osu!'s own hitsounds instead of the beatmap's
    builtin_sounds: bool,
    device: MixerDeviceSink,
    /// `None` while nothing is playing or the next song is still loading
    player: Option<Player>,
    /// The current song's audio couldn't be opened (e.g. an unsupported format)
    load_failed: bool,
    duration: Duration,
    /// Main tempo of the current song, read when it starts playing
    bpm: Option<f64>,
    /// Difficulties of the current song (indices into its charts), from easiest to hardest
    charts: Vec<usize>,
    /// Star rating of each of the current song's charts
    stars: Vec<f64>,
    /// Preferred difficulty category (an index into `library::CATEGORIES`) for picking a song's difficulty
    preferred: usize,
    /// The difficulty drum mode plays the hitsounds of
    chart: Option<usize>,
    /// Hands another difficulty's hitsounds to the playing source, with the music's format to decode them in
    swap: Option<(Sender<Hits>, ChannelCount, SampleRate)>,
    /// Hitsounds being decoded for a newly picked difficulty; replacing it cancels the previous pick
    picking: Task<()>,
    charts_scroll: ScrollHandle,
    /// The difficulty tags just appeared; scroll the selected one (the busiest, at the end) into view
    reveal_chart: bool,
    volume: f32,
    /// The user is dragging the progress bar, so the timer must not move it
    seeking: bool,
    /// The pending song load; replacing it cancels the previous one
    loading: Task<()>,

    // Video mode
    video_mode: bool,
    /// FFmpeg was found, so video mode can be used
    ffmpeg: bool,
    video: Option<Video>,
    /// Video frames no longer on screen, waiting to be freed from the GPU on the next render
    stale_frames: Vec<Arc<RenderImage>>,
    /// Last mouse move, scroll or key press; after a few idle seconds the UI fades out over the video
    last_input: Instant,
    /// Opacity of the UI on top of the video, 0 once it has faded out completely
    ui_opacity: f32,
    /// When `ui_opacity` was last advanced
    last_frame: Instant,
    /// Last mouse position seen, to ignore the move events repeated while the mouse stands still
    mouse: Point<Pixels>,
    /// The mouse pointer is hidden because the UI has faded out
    cursor_hidden: bool,

    // Widgets
    search: Entity<InputState>,
    difficulty: Entity<SelectState<Vec<&'static str>>>,
    progress: Entity<SliderState>,
    volume_slider: Entity<SliderState>,
    list: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl MusicPlayer {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("search")));
        let progress = cx.new(|_| SliderState::new().max(1.).step(0.001));
        let volume_slider = cx.new(|_| SliderState::new().max(1.).step(0.01).default_value(0.8));
        let difficulty = cx.new(|cx| SelectState::new(CATEGORIES.to_vec(), Some(IndexPath::new(CATEGORIES.len() - 1)), window, cx));

        let _subscriptions = vec![
            cx.subscribe(&difficulty, |this, _, e: &SelectEvent<Vec<&'static str>>, cx| {
                let SelectEvent::Confirm(Some(category)) = e else { return };
                this.prefer(CATEGORIES.iter().position(|c| c == category).unwrap_or(CATEGORIES.len() - 1), cx);
            }),
            cx.subscribe(&search, |this, _, e: &InputEvent, cx| {
                if matches!(e, InputEvent::Change) {
                    this.filter(cx);
                }
            }),
            // Seek only on release, so dragging doesn't restart the decoder on every move
            cx.subscribe(&progress, |this, _, e: &SliderEvent, _| match e {
                SliderEvent::Change(_) => this.seeking = true,
                SliderEvent::Release(v) => {
                    this.seeking = false;
                    if let Some(p) = &this.player {
                        let pos = this.duration.mul_f32(v.start());
                        p.try_seek(pos).ok();
                        this.start_video(pos);
                    }
                }
            }),
            cx.observe_window_appearance(window, |_, window, cx| apply_theme(window, cx)),
            cx.subscribe(&volume_slider, |this, _, (SliderEvent::Change(v) | SliderEvent::Release(v)), _| {
                this.volume = v.start();
                if let Some(p) = &this.player {
                    p.set_volume(this.volume);
                }
            }),
        ];

        // Refresh the progress bar and notice when a song ends
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                if this.update_in(cx, |this, window, cx| this.tick(window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut device = DeviceSinkBuilder::open_default_sink().expect(&t!("no_audio_device"));
        device.log_on_drop(false);

        let mut this = Self {
            songs: vec![],
            shown: vec![],
            scanning: false,
            current: None,
            history: vec![],
            mode: Mode::Order,
            drums: Arc::new(AtomicBool::new(false)),
            builtin_sounds: false,
            device,
            player: None,
            load_failed: false,
            duration: Duration::ZERO,
            bpm: None,
            charts: vec![],
            stars: vec![],
            preferred: CATEGORIES.len() - 1,
            chart: None,
            swap: None,
            picking: Task::ready(()),
            charts_scroll: ScrollHandle::new(),
            reveal_chart: false,
            volume: 0.8,
            seeking: false,
            loading: Task::ready(()),
            video_mode: false,
            ffmpeg: video::available(),
            video: None,
            stale_frames: vec![],
            last_input: Instant::now(),
            ui_opacity: 1.,
            last_frame: Instant::now(),
            mouse: Point::default(),
            cursor_hidden: false,
            search,
            difficulty,
            progress,
            volume_slider,
            list: UniformListScrollHandle::new(),
            _subscriptions,
        };
        this.scan(cx);
        this
    }

    // ---------- Library ----------

    fn scan(&mut self, cx: &mut Context<Self>) {
        self.scanning = true;
        cx.spawn(async move |this, cx| {
            let songs = cx.background_spawn(async { library::scan() }).await;
            this.update(cx, |this, cx| {
                // Indices change after a rescan; keep pointing at the song that's playing
                let audio = this.current.map(|i| this.songs[i].audio.clone());
                this.current = audio.and_then(|a| songs.iter().position(|s| s.audio == a));
                this.history.clear();
                this.songs = songs;
                this.scanning = false;
                this.filter(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn add_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                paths.iter().for_each(|p| library::add_folder(p));
                this.update(cx, |this, cx| this.scan(cx)).ok();
            }
        })
        .detach();
    }

    /// Every whitespace-separated word of the query must appear in the song's search key.
    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().to_lowercase();
        self.shown = (0..self.songs.len())
            .filter(|&i| query.split_whitespace().all(|w| self.songs[i].key.contains(w)))
            .collect();
        cx.notify();
    }

    // ---------- Playback ----------

    fn play(&mut self, ix: usize, cx: &mut Context<Self>) {
        // Replaying the same song (e.g. on repeat) keeps the difficulty that was picked
        let keep = self.chart.filter(|_| self.current == Some(ix));
        self.current = Some(ix);
        self.player = None;
        self.load_failed = false;
        self.bpm = None;
        self.charts.clear();
        self.stars.clear();
        self.chart = None;
        self.swap = None;
        if let Some(pos) = self.shown.iter().position(|&i| i == ix) {
            self.list.scroll_to_item(pos, ScrollStrategy::Nearest);
        }

        // Rating the difficulties and decoding the hitsound samples take a moment, so do it off the UI thread
        let (song, on, preferred, builtin) = (self.songs[ix].clone(), self.drums.clone(), self.preferred, self.builtin_sounds);
        self.loading = cx.spawn(async move |this, cx| {
            let (opened, stars, chart, bpm) = cx
                .background_spawn(async move {
                    let stars: Vec<f64> = song.charts.par_iter().map(|c| library::stars(&c.path)).collect();
                    let chart = keep.unwrap_or_else(|| library::pick(&stars, preferred));
                    let bpm = drums::bpm(&song.charts[chart].path);
                    (drums::open(&song, chart, builtin, on), stars, chart, bpm)
                })
                .await;
            this.update(cx, |this, cx| {
                this.bpm = bpm;
                let mut charts: Vec<usize> = (0..stars.len()).collect();
                charts.sort_by(|&a, &b| stars[a].total_cmp(&stars[b]));
                (this.charts, this.stars, this.chart, this.reveal_chart) = (charts, stars, Some(chart), true);
                this.load_failed = opened.is_none();
                if let Some((source, swap)) = opened {
                    this.swap = Some((swap, source.channels(), source.sample_rate()));
                    let player = Player::connect_new(this.device.mixer());
                    player.set_volume(this.volume);
                    this.duration = source.total_duration().unwrap_or_default();
                    player.append(source);
                    this.player = Some(player);
                }
                this.start_video(Duration::ZERO);
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// A new preferred category also re-picks the difficulty of the song that's playing.
    fn prefer(&mut self, category: usize, cx: &mut Context<Self>) {
        self.preferred = category;
        if !self.stars.is_empty() {
            self.pick_chart(library::pick(&self.stars, category), cx);
            self.reveal_chart = true;
        }
    }

    /// Switches drum mode to another difficulty's hitsounds without interrupting the music.
    /// Also used to reload the current difficulty's hitsounds after switching sound packs.
    fn pick_chart(&mut self, chart: usize, cx: &mut Context<Self>) {
        let (Some(ix), Some((swap, channels, rate))) = (self.current, self.swap.clone()) else { return };
        self.chart = Some(chart);
        let (song, builtin) = (self.songs[ix].clone(), self.builtin_sounds);
        self.picking = cx.background_spawn(async move {
            swap.send(drums::hits(&song, chart, builtin, channels, rate)).ok();
        });
        cx.notify();
    }

    /// (Re)starts the current song's video at music position `from`, or stops it when
    /// video mode is off or the song has no video.
    fn start_video(&mut self, from: Duration) {
        if let Some(mut old) = self.video.take() {
            self.stale_frames.extend(old.frame.take());
        }
        let video = self.current.and_then(|i| self.songs[i].video.clone());
        if self.video_mode && self.ffmpeg {
            self.video = video.and_then(|(path, offset)| Video::open(&path, offset, from.as_secs_f64() * 1000.));
        }
    }

    /// Any user input: brings the UI back if it faded out over the video.
    fn wake(&mut self, cx: &mut Context<Self>) {
        self.last_input = Instant::now();
        if self.ui_opacity < 1. {
            cx.notify();
        }
    }

    /// Move events also arrive with every redraw while the mouse stands still, so only real moves count.
    fn mouse_moved(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if e.position != self.mouse {
            self.mouse = e.position;
            self.wake(cx);
        }
    }

    /// The UI hides while a video plays and nothing has been touched for a few seconds.
    fn idle(&self) -> bool {
        self.video.as_ref().is_some_and(|v| v.frame.is_some())
            && self.player.as_ref().is_some_and(|p| !p.is_paused())
            && self.last_input.elapsed() > Duration::from_secs(3)
    }

    /// `auto` is true when the current song finished on its own (only then does Repeat replay it).
    fn next(&mut self, auto: bool, cx: &mut Context<Self>) {
        if self.shown.is_empty() {
            return;
        }
        let pos = self.current.and_then(|c| self.shown.iter().position(|&i| i == c));
        let ix = match (self.mode, self.current) {
            (Mode::Repeat, Some(c)) if auto => c,
            (Mode::Shuffle, _) => self.shown[rand::random_range(0..self.shown.len())],
            _ => self.shown[pos.map_or(0, |p| (p + 1) % self.shown.len())],
        };
        self.history.extend(self.current);
        self.play(ix, cx);
    }

    fn prev(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.history.pop() {
            self.play(ix, cx);
        }
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        match &self.player {
            Some(p) if p.is_paused() => p.play(),
            Some(p) => p.pause(),
            None => self.next(false, cx),
        }
        cx.notify();
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Done here rather than when the tags appear: the scroll position needs their first layout
        if std::mem::take(&mut self.reveal_chart) {
            let pos = self.charts.iter().position(|&i| Some(i) == self.chart).unwrap_or(0);
            self.charts_scroll.scroll_to_item(pos);
            cx.notify();
        }
        let Some(p) = &self.player else { return };
        if p.empty() {
            return self.next(true, cx);
        }
        if !self.seeking && !self.duration.is_zero() {
            let v = p.get_pos().as_secs_f32() / self.duration.as_secs_f32();
            self.progress.update(cx, |s, cx| s.set_value(v.min(1.), window, cx));
        }
        if !p.is_paused() {
            cx.notify();
        }
    }

    // ---------- Rendering ----------

    /// Left pane: search bar, column headers and the song list.
    fn render_library(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let header = columns(
            "#",
            SharedString::from(t!("col_title")),
            SharedString::from(t!("col_artist")),
            SharedString::from(t!("col_mapper")),
        )
        .h(px(32.))
        .text_xs()
        .text_color(theme.muted_foreground);

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_toolbar(cx))
            .child(div().px_2().border_b_1().border_color(theme.border).child(header))
            .child(self.render_list(cx))
    }

    /// Search box, song count and library buttons.
    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .px_4()
            .py_3()
            .gap_2()
            .child(Input::new(&self.search).cleanable(true).prefix(IconName::Search))
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(t!("song_count", n = self.shown.len()))),
            )
            .child(
                Button::new("add")
                    .ghost()
                    .icon(IconName::FolderPlus)
                    .tooltip(t!("add_folder"))
                    .on_click(cx.listener(|this, _, _, cx| this.add_folder(cx))),
            )
            .child(
                Button::new("rescan")
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .tooltip(t!("rescan"))
                    .loading(self.scanning)
                    .on_click(cx.listener(|this, _, _, cx| this.scan(cx))),
            )
    }

    /// The song list; only visible rows are rendered.
    fn render_list(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if self.shown.is_empty() {
            let text = if self.scanning { t!("scanning") } else { t!("no_songs") };
            return v_flex()
                .flex_1()
                .gap_3()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::ListMusic).size(px(40.)))
                .child(SharedString::from(text))
                .into_any_element();
        }

        uniform_list(
            "songs",
            self.shown.len(),
            cx.processor(|this, range: Range<usize>, _, cx| {
                let theme = cx.theme();
                range
                    .map(|i| {
                        let ix = this.shown[i];
                        let song = &this.songs[ix];
                        let current = this.current == Some(ix);

                        // The playing row swaps its number for an icon and highlights the title
                        let index = if current {
                            Icon::new(IconName::AudioLines).small().text_color(theme.primary).into_any_element()
                        } else {
                            (i + 1).to_string().into_any_element()
                        };
                        // A small film icon after the title marks songs that have a video
                        let title = h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(if current { theme.primary } else { theme.foreground })
                                    .when(current, |d| d.font_medium())
                                    .child(song.title.clone()),
                            )
                            .when(song.video.is_some(), |d| {
                                d.child(Icon::new(IconName::Film).size(px(14.)).flex_none().text_color(theme.muted_foreground))
                            });

                        columns(index, title, song.artist.clone(), song.creator.clone())
                            .id(i)
                            .h(px(40.))
                            .rounded_md()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .cursor_pointer()
                            .when(current, |d| d.bg(theme.list_active))
                            .hover(|d| d.bg(theme.list_hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.history.extend(this.current);
                                this.play(ix, cx);
                            }))
                    })
                    .collect()
            }),
        )
        .flex_1()
        .px_2()
        .py_1()
        .track_scroll(&self.list)
        .into_any_element()
    }

    /// Right pane: cover art, song info and every playback control.
    fn render_now_playing(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let song = self.current.map(|i| &self.songs[i]);
        let cover = song.and_then(|s| s.cover.as_deref());

        // The beatmap background, or an osu!-style gradient when there is none
        let art = match cover {
            // Absolutely positioned: in normal flow an <img> keeps its own aspect ratio and would overflow the card
            Some(path) => img(path).absolute().top_0().left_0().size_full().rounded_xl().object_fit(ObjectFit::Cover).into_any_element(),
            None => div()
                .size_full()
                .rounded_xl()
                .flex()
                .items_center()
                .justify_center()
                .bg(linear_gradient(135., linear_color_stop(rgb(0xff66aa), 0.), linear_color_stop(rgb(0x7c5cff), 1.)))
                .child(Icon::new(IconName::Music).size(px(48.)).text_color(white()))
                .into_any_element(),
        };

        let info = match song {
            Some(song) => v_flex()
                .gap_1()
                .child(div().text_xl().font_semibold().line_clamp(2).child(song.title.clone()))
                .child(div().truncate().text_color(theme.muted_foreground).child(song.artist.clone()))
                .when(self.load_failed, |d| d.child(div().text_sm().text_color(theme.danger).child(SharedString::from(t!("cannot_play")))))
                // Everything below keeps its height while the song loads, so the panel doesn't jump:
                // the chips stay on one line (a long mapper name is cut short instead of wrapping) and the
                // difficulty row is there from the start, filling in once the stars are calculated
                .child(
                    h_flex()
                        .pt_2()
                        .gap_2()
                        .child(chip(t!("mapped_by", name = song.creator), cx).min_w_0().truncate())
                        .when_some(self.bpm, |d, bpm| d.child(chip(t!("bpm", n = bpm.round()), cx).flex_none())),
                )
                .child(self.render_charts(song, cx)),
            None => v_flex()
                .gap_1()
                .child(div().text_xl().font_semibold().child("osu! music"))
                .child(div().text_color(theme.muted_foreground).child(SharedString::from(t!("idle")))),
        };

        let pos = self.player.as_ref().map(Player::get_pos).unwrap_or_default();
        let time = |d: Duration| format!("{:02}:{:02}", d.as_secs() / 60, d.as_secs() % 60);
        let progress = v_flex().gap_1().child(Slider::new(&self.progress)).child(
            h_flex()
                .justify_between()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(time(pos))
                .child(time(self.duration)),
        );

        // While a video plays behind the UI, the panel turns translucent and drops its own backdrop
        let video = self.video.is_some();
        v_flex()
            .relative()
            .w(px(340.))
            .flex_none()
            .h_full()
            .overflow_hidden()
            .border_l_1()
            .border_color(theme.border)
            .bg(if video { theme.sidebar.opacity(0.5) } else { theme.sidebar })
            // A faded copy of the cover fills the top of the panel, fading into the background
            .when_some(cover.filter(|_| !video), |d, path| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w_full()
                        .h(px(360.))
                        .child(img(path).absolute().top_0().left_0().size_full().object_fit(ObjectFit::Cover).opacity(0.35))
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .bg(linear_gradient(180., linear_color_stop(theme.sidebar.opacity(0.2), 0.), linear_color_stop(theme.sidebar, 1.))),
                        ),
                )
            })
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .px_6()
                    .py_8()
                    .gap_8()
                    // Three groups: the song and playback centred in the space above, settings at the bottom
                    .child(
                        v_flex()
                            .flex_1()
                            .justify_center()
                            .gap_8()
                            .child(
                                v_flex()
                                    .gap_5()
                                    .child(div().relative().w_full().h(px(164.)).rounded_xl().shadow_lg().child(art))
                                    .child(info),
                            )
                            .child(v_flex().gap_3().child(progress).child(self.render_transport(cx))),
                    )
                    .child(
                        v_flex().gap_3().child(self.render_options(cx)).child(
                            Select::new(&self.difficulty)
                                .id("difficulty")
                                .title_prefix(t!("default_difficulty"))
                                .w_full(),
                        ),
                    ),
            )
    }

    /// Difficulty tags, easiest first: drum mode plays the hitsounds of the selected one.
    /// A single fixed-height row that scrolls sideways (with the mouse wheel too), so big sets don't
    /// push the controls away and it can hold its place, empty, while the song loads.
    fn render_charts(&self, song: &Song, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .id("charts")
            .w_full()
            .h(px(28.))
            .mt_1()
            .gap_1p5()
            .overflow_x_scroll()
            .track_scroll(&self.charts_scroll)
            .children(self.charts.iter().map(|&i| {
                let (selected, stars) = (self.chart == Some(i), self.stars[i]);
                // A dot in the category's colour (osu-web's palette), then the name and star rating
                let dot = div()
                    .size(px(8.))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(rgb([0x88b300, 0x66ccff, 0xffcc22, 0xff66aa, 0x8866ee, 0x000000][library::category(stars)]));
                h_flex()
                    .id(("chart", i))
                    .flex_none()
                    .gap_1p5()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if selected { theme.primary } else { theme.border })
                    .when(selected, |d| d.bg(theme.list_active))
                    .text_xs()
                    .text_color(if selected { theme.primary } else { theme.muted_foreground })
                    .cursor_pointer()
                    .child(dot)
                    .child(song.charts[i].name.clone())
                    .child(div().opacity(0.7).child(format!("{stars:.2}★")))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_chart(i, cx)))
            }))
    }

    /// Previous, play / pause and next.
    fn render_transport(&self, cx: &Context<Self>) -> impl IntoElement {
        let playing = self.player.as_ref().is_some_and(|p| !p.is_paused());
        h_flex()
            .justify_center()
            .gap_6()
            .child(
                Button::new("prev")
                    .ghost()
                    .large()
                    .icon(IconName::SkipBack)
                    .tooltip(t!("prev"))
                    .on_click(cx.listener(|this, _, _, cx| this.prev(cx))),
            )
            .child(
                Button::new("play")
                    .primary()
                    .with_size(px(36.))
                    .size(px(56.))
                    .rounded(ButtonRounded::Size(px(28.)))
                    .icon(if playing { IconName::Pause } else { IconName::Play })
                    .tooltip(if playing { t!("pause") } else { t!("play") })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle(cx))),
            )
            .child(
                Button::new("next")
                    .ghost()
                    .large()
                    .icon(IconName::SkipForward)
                    .tooltip(t!("next"))
                    .on_click(cx.listener(|this, _, _, cx| this.next(false, cx))),
            )
    }

    /// Play mode, drum mode and video mode toggles, plus volume.
    fn render_options(&self, cx: &Context<Self>) -> impl IntoElement {
        let (mode_icon, mode_tip) = match self.mode {
            Mode::Repeat => (IconName::Repeat1, t!("mode_repeat")),
            Mode::Order => (IconName::Repeat, t!("mode_order")),
            Mode::Shuffle => (IconName::Shuffle, t!("mode_shuffle")),
        };
        let video_tip = if self.ffmpeg { t!("video_mode") } else { t!("no_ffmpeg") };

        h_flex()
            .gap_1()
            .text_color(cx.theme().muted_foreground)
            .child(
                Button::new("mode")
                    .ghost()
                    .icon(mode_icon)
                    .tooltip(mode_tip)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.mode = this.mode.next();
                        cx.notify();
                    })),
            )
            .child(
                toggle_button("drums", IconName::Drum, self.drums.load(Ordering::Relaxed), cx)
                    .tooltip(t!("drums"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.drums.fetch_xor(true, Ordering::Relaxed);
                        cx.notify();
                    })),
            )
            .child(
                toggle_button("video", IconName::Film, self.video_mode, cx)
                    .tooltip(video_tip)
                    .on_click(cx.listener(|this, _, window, cx| {
                        // Look again while it's missing, so installing FFmpeg takes effect without a restart
                        this.ffmpeg = this.ffmpeg || video::available();
                        if !this.ffmpeg {
                            return ffmpeg_dialog(window, cx);
                        }
                        this.video_mode = !this.video_mode;
                        let pos = this.player.as_ref().map(Player::get_pos).unwrap_or_default();
                        this.start_video(pos);
                        cx.notify();
                    })),
            )
            .child(
                toggle_button("sounds", IconName::Package, self.builtin_sounds, cx)
                    .tooltip(t!("builtin_sounds"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.builtin_sounds = !this.builtin_sounds;
                        // Swap the playing song's hitsounds right away
                        if let Some(chart) = this.chart {
                            this.pick_chart(chart, cx);
                        }
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .flex_1()
                    .ml_3()
                    .gap_3()
                    .child(Icon::new(if self.volume == 0. { IconName::VolumeX } else { IconName::Volume2 }).small())
                    .child(Slider::new(&self.volume_slider).flex_1()),
            )
    }
}

/// Explains that video mode needs FFmpeg and how to install it.
fn ffmpeg_dialog(window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, |dialog, _, cx| {
        let theme = cx.theme();
        let command = h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.secondary)
            .child(div().flex_1().font_family(theme.mono_font_family.clone()).child(video::INSTALL_COMMAND))
            .child(Clipboard::new("copy-install").value(video::INSTALL_COMMAND));

        dialog
            .title(SharedString::from(t!("ffmpeg_title")))
            .child(
                v_flex()
                    .gap_3()
                    .child(SharedString::from(t!("ffmpeg_body")))
                    .child(command)
                    .child(div().text_sm().text_color(theme.muted_foreground).child(SharedString::from(t!("ffmpeg_after")))),
            )
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label(t!("close"))))
                    .child(
                        Button::new("ffmpeg-download")
                            .primary()
                            .label(t!("ffmpeg_download"))
                            .on_click(|_, _, cx| cx.open_url(video::DOWNLOAD_URL)),
                    ),
            )
    });
}

/// Hides the mouse pointer while it is over our window, or shows it again. GPUI has no API for this,
/// so it goes to the OS directly. Not supported on Linux, where GPUI owns the X11 / Wayland cursor.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(unused_variables))]
fn set_cursor_hidden(hidden: bool) {
    // A per-thread display counter: other apps' windows keep their pointer, and the cursor shapes
    // GPUI keeps setting on every mouse message don't bring it back
    #[cfg(windows)]
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::ShowCursor(!hidden);
    }
    // macOS brings the pointer back by itself on the next mouse move
    #[cfg(target_os = "macos")]
    if hidden {
        objc2_app_kit::NSCursor::setHiddenUntilMouseMoves(true);
    }
}

/// An icon button that turns pink while `on`.
fn toggle_button(id: &'static str, icon: IconName, on: bool, cx: &App) -> Button {
    let theme = cx.theme();
    Button::new(id)
        .ghost()
        .icon(Icon::new(icon).when(on, |i| i.text_color(theme.primary)))
        .when(on, |b| b.bg(theme.list_active))
}

/// Shared cell layout of the header and the song rows: index, title, artist, mapper.
fn columns(index: impl IntoElement, title: impl IntoElement, artist: impl IntoElement, mapper: impl IntoElement) -> Div {
    h_flex()
        .w_full()
        .px_3()
        .gap_3()
        .child(div().w(px(28.)).flex_none().flex().justify_end().child(index))
        .child(div().flex_1().min_w_0().truncate().child(title))
        .child(div().w(relative(0.28)).flex_none().truncate().child(artist))
        .child(div().w(px(140.)).flex_none().truncate().child(mapper))
}

/// A small rounded label in the Now Playing panel.
fn chip(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(cx.theme().secondary)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// Follows the system light / dark mode, tinted with osu!'s signature pink.
fn apply_theme(window: &mut Window, cx: &mut App) {
    Theme::sync_system_appearance(Some(window), cx);
    let pink: Hsla = rgb(0xff66aa).into();
    Theme::update(cx, |t| {
        (t.primary, t.button_primary) = (pink, pink);
        (t.primary_hover, t.button_primary_hover) = (Hsla { l: 0.76, ..pink }, Hsla { l: 0.76, ..pink });
        (t.primary_active, t.button_primary_active) = (Hsla { l: 0.64, ..pink }, Hsla { l: 0.64, ..pink });
        (t.primary_foreground, t.button_primary_foreground) = (white(), white());
        (t.slider_bar, t.slider_thumb, t.progress_bar) = (pink, white(), pink);
        (t.ring, t.list_active, t.list_active_border) = (pink, pink.opacity(0.12), pink);
        t.selection = pink.opacity(0.35);
    });
}

impl Render for MusicPlayer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pick the video frame for this moment and keep redrawing while the music plays
        let pos = self.player.as_ref().map(Player::get_pos).unwrap_or_default();
        if let Some(video) = &mut self.video {
            let ms = pos.as_secs_f64() * 1000.;
            self.stale_frames.extend(video.advance(ms));
            if video.lagging(ms) {
                self.start_video(pos);
            }
            if self.player.as_ref().is_some_and(|p| !p.is_paused()) {
                window.request_animation_frame();
            }
        }
        // Every frame is a new texture, so free the ones that are no longer on screen
        for frame in self.stale_frames.drain(..) {
            cx.drop_image(frame, Some(window));
        }

        // Once idle, fade the UI (and the scrim) out over a second until only the video is left;
        // on input, fade it back in within a fifth of a second
        let idle = self.idle();
        let target = if idle { 0. } else { 1. };
        let step = self.last_frame.elapsed().as_secs_f32().min(0.1);
        self.last_frame = Instant::now();
        self.ui_opacity = if idle { (self.ui_opacity - step).max(0.) } else { (self.ui_opacity + step * 5.).min(1.) };
        if self.ui_opacity != target {
            window.request_animation_frame();
        }
        // Once the UI is gone, hide the pointer too; it comes back with the first real mouse move
        let hide_cursor = idle && self.ui_opacity == 0.;
        if hide_cursor != self.cursor_hidden {
            self.cursor_hidden = hide_cursor;
            set_cursor_hidden(hide_cursor);
        }

        let theme = cx.theme();
        let frame = self.video.as_ref().and_then(|v| v.frame.clone());
        // Light themes need a stronger scrim, as dark video areas would otherwise swallow grey text
        let scrim = if theme.is_dark() { 0.78 } else { 0.85 } * self.ui_opacity;
        div()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_mouse_move(cx.listener(|this, e, _, cx| this.mouse_moved(e, cx)))
            .on_scroll_wheel(cx.listener(|this, _, _, cx| this.wake(cx)))
            .on_key_down(cx.listener(|this, _, _, cx| this.wake(cx)))
            // Video mode: the video fills the window behind a scrim in the theme's background colour
            .when_some(frame, |d, frame| {
                d.child(img(frame).absolute().top_0().left_0().size_full().object_fit(ObjectFit::Cover))
                    .child(div().absolute().top_0().left_0().size_full().bg(theme.background.opacity(scrim)))
            })
            .child(
                h_flex()
                    .relative()
                    .size_full()
                    .opacity(self.ui_opacity)
                    .child(self.render_library(cx))
                    .child(self.render_now_playing(cx)),
            )
            // While hidden, a click only brings the UI back instead of reaching the invisible controls
            .when(idle, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .occlude()
                        .on_mouse_move(cx.listener(|this, e, _, cx| this.mouse_moved(e, cx)))
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.wake(cx))),
                )
            })
    }
}

fn main() {
    // Follow the system language: Chinese for zh-*, English otherwise.
    // The locale is global, so gpui-component's built-in texts follow it as well.
    let zh = sys_locale::get_locale().is_some_and(|l| l.starts_with("zh"));
    rust_i18n::set_locale(if zh { "zh-CN" } else { "en" });

    application().with_assets(AllAssets).run(|cx| {
        gpui_kit::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1120.), px(720.)), cx))),
            window_min_size: Some(size(px(860.), px(640.))),
            titlebar: Some(TitlebarOptions {
                title: Some("osu! music".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            apply_theme(window, cx);
            cx.new(|cx| MusicPlayer::new(window, cx))
        })
        .expect(&t!("no_window"));
        cx.on_window_closed(|cx, _| cx.quit()).detach();
    });
}
