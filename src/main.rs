//! osu! music: plays the songs in your osu!stable / osu!lazer library like a regular music player,
//! with an optional drum mode that adds the hitsounds an Auto play would make.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod drums;
mod library;
mod realm;

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui_kit::{
    assets::{AllAssets, IconName},
    component::{
        ActiveTheme, Icon, Sizable, StyledExt, Theme,
        button::{Button, ButtonRounded, ButtonVariants},
        h_flex,
        input::{Input, InputEvent, InputState},
        slider::{Slider, SliderEvent, SliderState},
        v_flex,
    },
    prelude::*,
    *,
};
use library::Song;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use rust_i18n::t;

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
    device: MixerDeviceSink,
    /// `None` while nothing is playing or the next song is still loading
    player: Option<Player>,
    duration: Duration,
    volume: f32,
    /// The user is dragging the progress bar, so the timer must not move it
    seeking: bool,
    /// The pending song load; replacing it cancels the previous one
    loading: Task<()>,

    // Widgets
    search: Entity<InputState>,
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

        let _subscriptions = vec![
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
                        p.try_seek(this.duration.mul_f32(v.start())).ok();
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
            device,
            player: None,
            duration: Duration::ZERO,
            volume: 0.8,
            seeking: false,
            loading: Task::ready(()),
            search,
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
        self.current = Some(ix);
        self.player = None;
        if let Some(pos) = self.shown.iter().position(|&i| i == ix) {
            self.list.scroll_to_item(pos, ScrollStrategy::Nearest);
        }

        // Decoding the hitsound samples can take a moment, so do it off the UI thread
        let (song, on) = (self.songs[ix].clone(), self.drums.clone());
        self.loading = cx.spawn(async move |this, cx| {
            let source = cx.background_spawn(async move { drums::open(&song, on) }).await;
            this.update(cx, |this, cx| {
                if let Some(source) = source {
                    let player = Player::connect_new(this.device.mixer());
                    player.set_volume(this.volume);
                    this.duration = source.total_duration().unwrap_or_default();
                    player.append(source);
                    this.player = Some(player);
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
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
                        let title = div()
                            .text_color(if current { theme.primary } else { theme.foreground })
                            .when(current, |d| d.font_medium())
                            .child(song.title.clone());

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
            Some(path) => img(path).size_full().rounded_xl().object_fit(ObjectFit::Cover).into_any_element(),
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
                .child(
                    h_flex()
                        .pt_2()
                        .gap_2()
                        .flex_wrap()
                        .child(chip(t!("mapped_by", name = song.creator), cx))
                        .child(chip(t!("difficulties", n = song.charts.len()), cx)),
                ),
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

        let volume = h_flex()
            .gap_3()
            .text_color(theme.muted_foreground)
            .child(Icon::new(if self.volume == 0. { IconName::VolumeX } else { IconName::Volume2 }).small())
            .child(Slider::new(&self.volume_slider).flex_1());

        v_flex()
            .relative()
            .w(px(340.))
            .flex_none()
            .h_full()
            .overflow_hidden()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            // A faded copy of the cover fills the top of the panel, fading into the background
            .when_some(cover, |d, path| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .w_full()
                        .h(px(360.))
                        .child(img(path).size_full().object_fit(ObjectFit::Cover).opacity(0.35))
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
                    .justify_center()
                    .px_6()
                    .py_8()
                    .gap_6()
                    .child(div().w_full().h(px(164.)).rounded_xl().shadow_lg().child(art))
                    .child(info)
                    .child(progress)
                    .child(self.render_controls(cx))
                    .child(volume),
            )
    }

    /// Play mode, previous, play / pause, next and drum mode.
    fn render_controls(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let playing = self.player.as_ref().is_some_and(|p| !p.is_paused());
        let drums = self.drums.load(Ordering::Relaxed);
        let (mode_icon, mode_tip) = match self.mode {
            Mode::Repeat => (IconName::Repeat1, t!("mode_repeat")),
            Mode::Order => (IconName::Repeat, t!("mode_order")),
            Mode::Shuffle => (IconName::Shuffle, t!("mode_shuffle")),
        };

        h_flex()
            .justify_between()
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
            .child(
                Button::new("drums")
                    .ghost()
                    .icon(Icon::new(IconName::Drum).when(drums, |i| i.text_color(theme.primary)))
                    .tooltip(t!("drums"))
                    .when(drums, |b| b.bg(theme.list_active))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.drums.fetch_xor(true, Ordering::Relaxed);
                        cx.notify();
                    })),
            )
    }
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_library(cx))
            .child(self.render_now_playing(cx))
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
            window_min_size: Some(size(px(860.), px(560.))),
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
