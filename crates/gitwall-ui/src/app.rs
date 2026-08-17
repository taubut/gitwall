//! The picker: state, input, and a render loop that only runs when something
//! is actually moving.
//!
//! One idea shapes the whole file: `rows` is the source list (a repo's images,
//! or your favourites), and `order` is the filtered-and-sorted list of indices
//! into it. `cursor` walks `order`, never `rows`. Textures are keyed by row, so
//! changing a filter re-orders what you see without throwing away a single
//! decoded image.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use egui::{
    Align2, Color32, Context, FontId, Key, Painter, Pos2, Rect, Sense, Shape, Stroke, TextureHandle,
    TextureOptions, Vec2, ViewportCommand,
};
use gitwall_core::{colour, Library, Swatch, ThumbMeta, Visit};

use crate::bridge::{Bridge, Cmd, Evt, Row};
use crate::geom::{self, Cover};
use crate::theme::{self, Fonts, Metrics};

const FULL_ZONE: f32 = 4.0;
const FADE_ZONE: f32 = 7.0;

const RENDER_WINDOW: i64 = 14;
const LOOKBEHIND: i64 = 8;
const LOOKAHEAD: i64 = 14;
/// How far either side of the cursor counts as "recently seen".
const TEXTURE_WINDOW: i64 = 36;
/// Texture budget. Browsing hundreds of wallpapers would otherwise pin
/// gigabytes of VRAM.
const SLICE_CAP: usize = 240;
const BACKDROP_CAP: usize = 48;

const GLIDE_TAU: f32 = 0.075;
const BACKDROP_FADE: f32 = 0.32;
const SETTLE: f64 = 0.28;
const SCROLL_STEP: f32 = 40.0;

/// How close to the end of the list triggers loading the next page.
const MORE_TRIGGER: i64 = 24;

const GRID_TILE_TARGET: f32 = 300.0;
const GRID_GAP: f32 = 10.0;

fn repo_field() -> egui::Id {
    egui::Id::new("gitwall-repo-field")
}

enum Screen {
    Empty,
    Working,
    Loaded,
    Fault(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Slices,
    Grid,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Repo,
    Favourites,
}

pub struct App {
    bridge: Bridge,
    fonts: Fonts,
    library: Library,
    screen: Screen,

    /// The repo currently loaded, kept even while browsing favourites so you
    /// can switch back without re-resolving.
    repo_rows: Vec<Row>,
    label: String,
    commit_short: String,
    total_bytes: u64,

    source: Source,
    view_mode: ViewMode,

    /// Active source list.
    rows: Vec<Row>,
    /// Indices into `rows`, filtered and sorted. The cursor walks this.
    order: Vec<i64>,
    cursor: i64,
    pos: f32,
    grid_scroll: f32,
    grid_scroll_target: f32,
    scroll_accum: f32,

    sections: Vec<String>,
    filter_dir: Option<String>,
    filter_swatch: Option<Swatch>,
    only_favs: bool,
    sort_colour: bool,

    slices: HashMap<i64, TextureHandle>,
    backdrops: HashMap<i64, TextureHandle>,
    metas: HashMap<i64, ThumbMeta>,
    requested: HashSet<i64>,
    full_requested: HashSet<i64>,
    /// Row -> tick it was last near the cursor, for texture eviction.
    seen: HashMap<i64, u64>,
    tick: u64,
    failed: HashSet<i64>,

    shown_backdrop: Option<i64>,
    prev_backdrop: Option<i64>,
    fade: f32,

    url: String,
    home_pick: usize,
    accent: Color32,
    toast: Option<(String, f64, bool)>,
    applying: bool,
    settle_deadline: Option<f64>,
    screen_rect: Rect,
    /// Where a paged source is up to; `None` once everything is loaded.
    paging: Option<crate::bridge::Paging>,
    search_total: u64,
    loading_more: bool,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        cache: gitwall_core::Cache,
        initial: Option<String>,
    ) -> Self {
        let fonts = theme::install_fonts(&cc.egui_ctx);
        let library = Library::open(cache.data_root().to_path_buf());
        let bridge = crate::bridge::spawn(cc.egui_ctx.clone(), cache);

        let mut app = Self {
            bridge,
            fonts,
            library,
            screen: Screen::Empty,
            repo_rows: Vec::new(),
            label: String::new(),
            commit_short: String::new(),
            total_bytes: 0,
            source: Source::Repo,
            view_mode: ViewMode::Slices,
            rows: Vec::new(),
            order: Vec::new(),
            cursor: 0,
            pos: 0.0,
            grid_scroll: 0.0,
            grid_scroll_target: 0.0,
            scroll_accum: 0.0,
            sections: Vec::new(),
            filter_dir: None,
            filter_swatch: None,
            only_favs: false,
            sort_colour: false,
            slices: HashMap::new(),
            backdrops: HashMap::new(),
            metas: HashMap::new(),
            requested: HashSet::new(),
            full_requested: HashSet::new(),
            seen: HashMap::new(),
            tick: 0,
            failed: HashSet::new(),
            shown_backdrop: None,
            prev_backdrop: None,
            fade: 1.0,
            url: String::new(),
            home_pick: 0,
            accent: theme::ACCENT_FALLBACK,
            toast: None,
            applying: false,
            settle_deadline: None,
            screen_rect: Rect::ZERO,
            paging: None,
            search_total: 0,
            loading_more: false,
        };

        if let Some(url) = initial {
            app.url = url.clone();
            app.load(url);
        }
        app
    }

    fn load(&mut self, url: String) {
        self.screen = Screen::Working;
        self.source = Source::Repo;
        self.reset_images();
        self.bridge.send(Cmd::Resolve(url));
    }

    /// Clears everything derived from a source list. Textures go too: they are
    /// keyed by row index, which is only meaningful within one source.
    fn reset_images(&mut self) {
        self.rows.clear();
        self.order.clear();
        self.slices.clear();
        self.backdrops.clear();
        self.metas.clear();
        self.requested.clear();
        self.full_requested.clear();
        self.seen.clear();
        self.failed.clear();
        self.shown_backdrop = None;
        self.prev_backdrop = None;
        self.cursor = 0;
        self.pos = 0.0;
        self.grid_scroll = 0.0;
        self.grid_scroll_target = 0.0;
        self.accent = theme::ACCENT_FALLBACK;
        self.paging = None;
        self.loading_more = false;
    }

    /* -------------------------------------------------------- source list */

    fn set_source(&mut self, source: Source) {
        if self.source == source {
            return;
        }
        self.source = source;
        self.reset_images();

        self.rows = match source {
            Source::Repo => self.repo_rows.clone(),
            Source::Favourites => self
                .library
                .favourites()
                .iter()
                .map(Row::from_favourite)
                .collect(),
        };

        // Favourites already are the favourites; the filter would be a no-op
        // that only confuses.
        if source == Source::Favourites {
            self.only_favs = false;
        }
        self.recompute_sections();
        self.rebuild_order();
        if !self.rows.is_empty() {
            self.screen = Screen::Loaded;
        }
    }

    fn recompute_sections(&mut self) {
        let mut dirs: Vec<String> = Vec::new();
        for r in &self.rows {
            if !r.group.is_empty() && !dirs.contains(&r.group) {
                dirs.push(r.group.clone());
            }
        }
        dirs.sort();
        self.sections = dirs;
        if let Some(d) = &self.filter_dir {
            if !self.sections.contains(d) {
                self.filter_dir = None;
            }
        }
    }

    fn swatch_of(&self, row: i64) -> Option<Swatch> {
        self.metas
            .get(&row)
            .map(|m| colour::classify(m.accent, m.mono))
    }

    /// Recompute `order` from the filters, keeping the same wallpaper focused
    /// where possible so changing a filter doesn't lose your place.
    fn rebuild_order(&mut self) {
        let anchor = self.focused_row();

        let mut order: Vec<i64> = (0..self.rows.len() as i64)
            .filter(|i| {
                let r = &self.rows[*i as usize];
                if let Some(d) = &self.filter_dir {
                    if &r.group != d {
                        return false;
                    }
                }
                if self.only_favs && !self.library.is_favourite(&r.key) {
                    return false;
                }
                if let Some(want) = self.filter_swatch {
                    // Unknown colour means the thumbnail hasn't been fetched;
                    // hide rather than guess.
                    match self.swatch_of(*i) {
                        Some(s) if s == want => {}
                        _ => return false,
                    }
                }
                true
            })
            .collect();

        if self.sort_colour {
            // Stable within a hue so the original order still shows through.
            order.sort_by_key(|i| {
                (
                    self.swatch_of(*i)
                        .map(|s| s.order_key())
                        .unwrap_or(u16::MAX),
                    self.rows[*i as usize].label.clone(),
                )
            });
        }

        self.order = order;
        self.cursor = anchor
            .and_then(|row| self.order.iter().position(|i| *i == row))
            .map(|p| p as i64)
            .unwrap_or(0)
            .min((self.order.len() as i64 - 1).max(0));
        self.pos = self.cursor as f32;
        self.ensure_grid_visible();
    }

    fn focused_row(&self) -> Option<i64> {
        self.order.get(self.cursor as usize).copied()
    }

    fn row_at(&self, slot: i64) -> Option<&Row> {
        self.order
            .get(slot as usize)
            .and_then(|i| self.rows.get(*i as usize))
    }

    fn filters_active(&self) -> bool {
        self.filter_dir.is_some() || self.filter_swatch.is_some() || self.only_favs
    }

    fn clear_filters(&mut self) {
        self.filter_dir = None;
        self.filter_swatch = None;
        self.only_favs = false;
        self.rebuild_order();
    }

    /* ------------------------------------------------------------ events -- */

    fn drain(&mut self, ctx: &Context) {
        while let Some(evt) = self.bridge.try_recv() {
            match evt {
                Evt::Resolved {
                    title,
                    badge,
                    input,
                    total_bytes,
                    truncated,
                    rows,
                    paging,
                } => {
                    self.label = title.clone();
                    self.commit_short = badge;
                    self.total_bytes = total_bytes;
                    self.repo_rows = rows.clone();
                    self.rows = rows;
                    self.source = Source::Repo;
                    self.screen = Screen::Loaded;
                    self.search_total = paging.as_ref().map(|p| p.total).unwrap_or(0);
                    self.paging = paging;
                    self.recompute_sections();
                    self.rebuild_order();

                    self.library.record(Visit {
                        input,
                        title,
                        images: self.rows.len(),
                        last_used: gitwall_core::library::now(),
                    });
                    let _ = self.library.save();

                    if truncated {
                        self.say("GitHub truncated the tree — some images are missing", true, ctx);
                    }
                }
                Evt::ResolveFailed(msg) => self.screen = Screen::Fault(msg),

                Evt::More { rows, paging } => {
                    self.loading_more = false;
                    self.paging = paging;
                    if !rows.is_empty() {
                        // Appending never disturbs existing indices, so loaded
                        // textures and the cursor stay put.
                        self.rows.extend(rows.iter().cloned());
                        if self.source == Source::Repo {
                            self.repo_rows.extend(rows);
                        }
                        self.commit_short = format!(
                            "wallhaven · {} of {} results",
                            self.rows.len(),
                            self.search_total
                        );
                        self.rebuild_order();
                    }
                }

                Evt::Thumb {
                    row,
                    slice,
                    backdrop,
                    meta,
                } => {
                    let i = row as i64;
                    self.slices.insert(
                        i,
                        ctx.load_texture(format!("s{i}"), (*slice).clone(), TextureOptions::LINEAR),
                    );
                    self.backdrops.insert(
                        i,
                        ctx.load_texture(
                            format!("b{i}"),
                            (*backdrop).clone(),
                            TextureOptions::LINEAR,
                        ),
                    );
                    self.metas.insert(i, meta);
                    self.failed.remove(&i);

                    // New colour data can change what a colour filter or sort
                    // should be showing.
                    if self.filter_swatch.is_some() || self.sort_colour {
                        self.rebuild_order();
                    }
                }
                Evt::ThumbFailed { row } => {
                    self.failed.insert(row as i64);
                }

                Evt::Full {
                    row,
                    slice,
                    backdrop,
                } => {
                    let i = row as i64;
                    self.slices.insert(
                        i,
                        ctx.load_texture(format!("sf{i}"), (*slice).clone(), TextureOptions::LINEAR),
                    );
                    self.backdrops.insert(
                        i,
                        ctx.load_texture(
                            format!("bf{i}"),
                            (*backdrop).clone(),
                            TextureOptions::LINEAR,
                        ),
                    );
                }

                Evt::Applied(name) => {
                    self.applying = false;
                    self.say(&format!("Wallpaper set — {name}"), false, ctx);
                }
                Evt::ApplyFailed(msg) => {
                    self.applying = false;
                    self.say(&msg, true, ctx);
                }
            }
        }
    }

    fn say(&mut self, msg: &str, bad: bool, ctx: &Context) {
        let now = ctx.input(|i| i.time);
        self.toast = Some((msg.to_string(), now + 2.8, bad));
    }

    /* ------------------------------------------------------------- input -- */

    fn columns(&self) -> i64 {
        let avail = (self.screen_rect.width() - 2.0 * self.pad()).max(200.0);
        (((avail + GRID_GAP) / (GRID_TILE_TARGET + GRID_GAP)).round() as i64).clamp(2, 10)
    }

    fn pad(&self) -> f32 {
        (self.screen_rect.width() * 0.032).clamp(20.0, 52.0)
    }

    fn handle_input(&mut self, ctx: &Context) {
        let typing = ctx.memory(|m| m.focused().is_some());

        if !typing && ctx.input(|i| i.key_pressed(Key::Slash)) {
            ctx.memory_mut(|m| m.request_focus(repo_field()));
            return;
        }

        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            if typing {
                ctx.memory_mut(|m| m.stop_text_input());
            } else {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            return;
        }

        if typing {
            return;
        }

        // Home screen: pick from history.
        if matches!(self.screen, Screen::Empty) {
            let n = self.library.history().len();
            if n > 0 {
                ctx.input(|i| {
                    if i.key_pressed(Key::ArrowDown) {
                        self.home_pick = (self.home_pick + 1).min(n - 1);
                    }
                    if i.key_pressed(Key::ArrowUp) {
                        self.home_pick = self.home_pick.saturating_sub(1);
                    }
                });
                if ctx.input(|i| i.key_pressed(Key::Enter)) {
                    let url = self.library.history()[self.home_pick].input.clone();
                    self.url = url.clone();
                    self.load(url);
                }
            }
            return;
        }

        if !matches!(self.screen, Screen::Loaded) {
            return;
        }

        let cols = self.columns();
        let grid = self.view_mode == ViewMode::Grid;

        let (mut delta, scroll, enter, fav, toggle_view, toggle_sort, toggle_source) =
            ctx.input(|i| {
                let mut step = 0i64;
                if i.key_pressed(Key::ArrowRight) {
                    step += 1;
                }
                if i.key_pressed(Key::ArrowLeft) {
                    step -= 1;
                }
                if i.key_pressed(Key::ArrowDown) {
                    step += if grid { cols } else { 10 };
                }
                if i.key_pressed(Key::ArrowUp) {
                    step -= if grid { cols } else { 10 };
                }
                if i.key_pressed(Key::PageDown) {
                    step += if grid { cols * 3 } else { 10 };
                }
                if i.key_pressed(Key::PageUp) {
                    step -= if grid { cols * 3 } else { 10 };
                }
                if i.key_pressed(Key::Home) {
                    step = -self.cursor;
                }
                if i.key_pressed(Key::End) {
                    step = self.order.len() as i64 - 1 - self.cursor;
                }
                let s = i.smooth_scroll_delta;
                (
                    step,
                    if s.x.abs() > s.y.abs() { s.x } else { s.y },
                    i.key_pressed(Key::Enter),
                    i.key_pressed(Key::F),
                    i.key_pressed(Key::G),
                    i.key_pressed(Key::S),
                    i.key_pressed(Key::Tab),
                )
            });

        if !ctx.egui_wants_pointer_input() {
            let click = ctx.input(|i| {
                i.pointer
                    .primary_clicked()
                    .then(|| i.pointer.interact_pos())
                    .flatten()
            });
            if let Some(p) = click {
                if let Some(slot) = self.hit(p) {
                    if slot == self.cursor {
                        self.apply();
                    } else {
                        self.set_cursor(slot);
                    }
                }
            }
        }

        self.scroll_accum -= scroll;
        while self.scroll_accum.abs() >= SCROLL_STEP {
            let dir = self.scroll_accum.signum();
            self.scroll_accum -= dir * SCROLL_STEP;
            delta += if grid { dir as i64 * cols } else { dir as i64 };
        }

        if delta != 0 {
            self.set_cursor(self.cursor + delta);
        }
        if enter {
            self.apply();
        }
        if fav {
            self.toggle_favourite(ctx);
        }
        if toggle_view {
            self.view_mode = match self.view_mode {
                ViewMode::Slices => ViewMode::Grid,
                ViewMode::Grid => ViewMode::Slices,
            };
            self.ensure_grid_visible();
        }
        if toggle_sort {
            self.sort_colour = !self.sort_colour;
            self.rebuild_order();
        }
        if toggle_source {
            let next = match self.source {
                Source::Repo => Source::Favourites,
                Source::Favourites => Source::Repo,
            };
            if next == Source::Favourites && self.library.favourites().is_empty() {
                self.say("No favourites yet — press F on a wallpaper", true, ctx);
            } else {
                self.set_source(next);
            }
        }
    }

    fn set_cursor(&mut self, slot: i64) {
        if self.order.is_empty() {
            return;
        }
        self.cursor = slot.clamp(0, self.order.len() as i64 - 1);
        self.settle_deadline = None;
        self.ensure_grid_visible();
    }

    fn ensure_grid_visible(&mut self) {
        if self.view_mode != ViewMode::Grid {
            return;
        }
        let cols = self.columns();
        let (tile_w, tile_h) = self.tile_size(cols);
        let _ = tile_w;
        let row = self.cursor / cols;
        let top = row as f32 * (tile_h + GRID_GAP);
        let view_h = self.grid_viewport().height();

        if top < self.grid_scroll_target {
            self.grid_scroll_target = top;
        } else if top + tile_h > self.grid_scroll_target + view_h {
            self.grid_scroll_target = top + tile_h - view_h;
        }
        self.grid_scroll_target = self.grid_scroll_target.max(0.0);
    }

    fn toggle_favourite(&mut self, ctx: &Context) {
        let Some(row) = self.focused_row() else { return };
        let Some(r) = self.rows.get(row as usize).cloned() else {
            return;
        };

        let now_starred = self.library.toggle_favourite(r.favourite());
        let _ = self.library.save();

        self.say(
            if now_starred {
                "Starred"
            } else {
                "Unstarred"
            },
            false,
            ctx,
        );

        // Unstarring while viewing favourites should remove it from the strip.
        if self.source == Source::Favourites && !now_starred {
            let keep = self.cursor;
            self.set_source_force(Source::Favourites);
            self.set_cursor(keep);
        } else if self.only_favs {
            self.rebuild_order();
        }
    }

    fn set_source_force(&mut self, source: Source) {
        self.source = match source {
            Source::Repo => Source::Favourites,
            Source::Favourites => Source::Repo,
        };
        self.set_source(source);
    }

    fn apply(&mut self) {
        if self.applying {
            return;
        }
        let Some(r) = self.row_at(self.cursor).cloned() else {
            return;
        };
        self.applying = true;
        self.bridge.send(Cmd::Apply {
            target: r.full_target(),
            name: r.name.clone(),
        });
    }

    /* --------------------------------------------------------- animation -- */

    fn animate(&mut self, ctx: &Context) -> bool {
        let dt = ctx.input(|i| i.stable_dt).clamp(0.0, 0.1);
        let k = 1.0 - (-dt / GLIDE_TAU).exp();

        let target = self.cursor as f32;
        self.pos += (target - self.pos) * k;
        if (target - self.pos).abs() < 0.0015 {
            self.pos = target;
        }

        self.grid_scroll += (self.grid_scroll_target - self.grid_scroll) * k;
        if (self.grid_scroll_target - self.grid_scroll).abs() < 0.4 {
            self.grid_scroll = self.grid_scroll_target;
        }

        if self.fade < 1.0 {
            self.fade = (self.fade + dt / BACKDROP_FADE).min(1.0);
        }

        self.pos != target || self.fade < 1.0 || self.grid_scroll != self.grid_scroll_target
    }

    fn sync_backdrop(&mut self) {
        let Some(want) = self.focused_row() else { return };
        if self.shown_backdrop == Some(want) || !self.backdrops.contains_key(&want) {
            return;
        }
        self.prev_backdrop = self.shown_backdrop;
        self.shown_backdrop = Some(want);
        self.fade = 0.0;
    }

    fn pump(&mut self, ctx: &Context) {
        if self.order.is_empty() {
            return;
        }
        let near = self.pos.round() as i64;
        let span = if self.view_mode == ViewMode::Grid {
            self.columns() * 4
        } else {
            LOOKAHEAD
        };
        let behind = if self.view_mode == ViewMode::Grid {
            self.columns() * 2
        } else {
            LOOKBEHIND
        };

        let lo = (near - behind).max(0);
        let hi = (near + span).min(self.order.len() as i64 - 1);

        for slot in lo..=hi {
            let Some(&row) = self.order.get(slot as usize) else {
                continue;
            };
            if self.slices.contains_key(&row) || self.requested.contains(&row) {
                continue;
            }
            self.requested.insert(row);
            if let Some(r) = self.rows.get(row as usize) {
                self.bridge.send(Cmd::Thumb {
                    row: row as usize,
                    target: r.preview_target(),
                });
            }
        }

        // Mark everything currently near the cursor as recently seen, then
        // evict the least-recently-seen if we are over budget.
        //
        // Evicting by *display distance* instead would throw away every loaded
        // texture the moment the order changes — sorting by hue reshuffles
        // which rows sit near the cursor, so a perfectly good set of images
        // would be dropped and immediately re-downloaded, blanking the view.
        self.tick += 1;
        for slot in (near - TEXTURE_WINDOW).max(0)
            ..=(near + TEXTURE_WINDOW).min(self.order.len() as i64 - 1)
        {
            if let Some(&row) = self.order.get(slot as usize) {
                self.seen.insert(row, self.tick);
            }
        }

        self.evict();

        // Pull the next page as the end of the list comes into view.
        if !self.loading_more && self.source == Source::Repo {
            if let Some(p) = self.paging.clone() {
                if self.cursor + MORE_TRIGGER >= self.order.len() as i64 {
                    self.loading_more = true;
                    self.bridge.send(Cmd::SearchMore {
                        query: p.query,
                        page: p.next_page,
                    });
                }
            }
        }

        if let Some(row) = self.focused_row() {
            if let Some(meta) = self.metas.get(&row) {
                let [r, g, b] = meta.accent;
                self.accent = Color32::from_rgb(r, g, b);
            }

            if self.pos == self.cursor as f32 && !self.full_requested.contains(&row) {
                let now = ctx.input(|i| i.time);
                match self.settle_deadline {
                    None => {
                        self.settle_deadline = Some(now + SETTLE);
                        ctx.request_repaint_after(Duration::from_secs_f64(SETTLE));
                    }
                    Some(t) if now >= t => {
                        self.full_requested.insert(row);
                        if let Some(r) = self.rows.get(row as usize) {
                            self.bridge.send(Cmd::Full {
                                row: row as usize,
                                target: r.full_target(),
                            });
                        }
                        self.settle_deadline = None;
                    }
                    Some(t) => ctx.request_repaint_after(Duration::from_secs_f64(t - now)),
                }
            }
        }
    }

    /* ------------------------------------------------------------ layout -- */

    /// Drop the least-recently-seen textures once over budget.
    ///
    /// Keyed on rows, never on positions, so re-ordering the view costs
    /// nothing. A slice texture is ~900 KB and a backdrop ~400 KB, so these
    /// caps are a few hundred MB at worst.
    fn evict(&mut self) {
        let trim = |map: &mut HashMap<i64, TextureHandle>, cap: usize, keep: [Option<i64>; 2]| {
            if map.len() <= cap {
                return Vec::new();
            }
            let mut ages: Vec<(u64, i64)> = map
                .keys()
                .filter(|k| !keep.contains(&Some(**k)))
                .map(|k| (*self.seen.get(k).unwrap_or(&0), *k))
                .collect();
            ages.sort_unstable();

            let mut dropped = Vec::new();
            for (_, row) in ages.into_iter().take(map.len().saturating_sub(cap)) {
                map.remove(&row);
                dropped.push(row);
            }
            dropped
        };

        for row in trim(&mut self.slices, SLICE_CAP, [None, None]) {
            // Let it be fetched again if it comes back into view.
            self.requested.remove(&row);
        }
        trim(
            &mut self.backdrops,
            BACKDROP_CAP,
            [self.shown_backdrop, self.prev_backdrop],
        );

        // Keep the recency map from growing without bound.
        if self.seen.len() > SLICE_CAP * 4 {
            let live: HashSet<i64> = self
                .slices
                .keys()
                .chain(self.backdrops.keys())
                .copied()
                .collect();
            self.seen.retain(|k, _| live.contains(k));
        }
    }

    fn bump(&self, slot: i64) -> f32 {
        (1.0 - (slot as f32 - self.pos).abs()).max(0.0)
    }

    fn width_at(&self, slot: i64, m: &Metrics) -> f32 {
        m.slice_w + (m.expanded - m.slice_w) * self.bump(slot)
    }

    fn x_at(&self, slot: i64, m: &Metrics) -> f32 {
        let k = self.pos.floor() as i64;
        let t = self.pos - k as f32;
        let extra = m.expanded - m.slice_w;
        let mut prefix = 0.0;
        if slot > k {
            prefix += 1.0 - t;
        }
        if slot > k + 1 {
            prefix += t;
        }
        slot as f32 * m.step() + extra * prefix
    }

    fn centre_x(&self, m: &Metrics) -> f32 {
        let k = self.pos.floor() as i64;
        let t = self.pos - k as f32;
        let a = self.x_at(k, m) + self.width_at(k, m) * 0.5;
        let b = self.x_at(k + 1, m) + self.width_at(k + 1, m) * 0.5;
        a + (b - a) * t
    }

    fn alpha_at(&self, slot: i64) -> f32 {
        let d = (slot as f32 - self.pos).abs();
        if d <= FULL_ZONE {
            1.0
        } else {
            (1.0 - (d - FULL_ZONE) / FADE_ZONE).max(0.0)
        }
    }

    fn tile_size(&self, cols: i64) -> (f32, f32) {
        let avail = self.screen_rect.width() - 2.0 * self.pad();
        let w = (avail - GRID_GAP * (cols - 1) as f32) / cols as f32;
        (w, w * 9.0 / 16.0)
    }

    fn grid_viewport(&self) -> Rect {
        let r = self.screen_rect;
        Rect::from_min_max(
            Pos2::new(r.left() + self.pad(), r.top() + 132.0),
            Pos2::new(r.right() - self.pad(), r.bottom() - 116.0),
        )
    }

    /// Which display slot is under a point, in whichever view is active.
    fn hit(&self, p: Pos2) -> Option<i64> {
        match self.view_mode {
            ViewMode::Grid => {
                let view = self.grid_viewport();
                if !view.contains(p) {
                    return None;
                }
                let cols = self.columns();
                let (tw, th) = self.tile_size(cols);
                let x = p.x - view.left();
                let y = p.y - view.top() + self.grid_scroll;
                let col = (x / (tw + GRID_GAP)).floor() as i64;
                let row = (y / (th + GRID_GAP)).floor() as i64;
                if col < 0 || col >= cols || row < 0 {
                    return None;
                }
                // Reject the gap between tiles so clicks feel precise.
                if x % (tw + GRID_GAP) > tw || y % (th + GRID_GAP) > th {
                    return None;
                }
                let slot = row * cols + col;
                (slot < self.order.len() as i64).then_some(slot)
            }
            ViewMode::Slices => {
                let m = Metrics::new(self.screen_rect.size());
                let top = self.screen_rect.center().y - m.slice_h * 0.5;
                if p.y < top || p.y > top + m.slice_h {
                    return None;
                }
                let offset = self.screen_rect.center().x - self.centre_x(&m);
                let near = self.pos.round() as i64;
                let mut best: Option<(f32, i64)> = None;
                for slot in (near - RENDER_WINDOW).max(0)
                    ..=(near + RENDER_WINDOW).min(self.order.len() as i64 - 1)
                {
                    if self.alpha_at(slot) <= 0.05 {
                        continue;
                    }
                    let t = (p.y - top) / m.slice_h;
                    let x0 = self.x_at(slot, &m) + offset + m.skew * (1.0 - t);
                    if p.x >= x0 && p.x <= x0 + self.width_at(slot, &m) {
                        let d = (slot as f32 - self.pos).abs();
                        if best.is_none_or(|(bd, _)| d < bd) {
                            best = Some((d, slot));
                        }
                    }
                }
                best.map(|(_, s)| s)
            }
        }
    }
}

/* ----------------------------------------------------------------- paint -- */

fn human_bytes(n: u64) -> String {
    if n == 0 {
        "—".into()
    } else if n >= 1_000_000_000 {
        format!("{:.1} GB", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else {
        format!("{} KB", n / 1000)
    }
}

/// Cover-fit UV rect for an axis-aligned tile.
fn cover_uv(img: Vec2, tile: Vec2) -> Rect {
    let img_a = img.x / img.y.max(1.0);
    let tile_a = tile.x / tile.y.max(1.0);
    if img_a > tile_a {
        let f = tile_a / img_a;
        Rect::from_min_max(Pos2::new(0.5 - f / 2.0, 0.0), Pos2::new(0.5 + f / 2.0, 1.0))
    } else {
        let f = img_a / tile_a;
        Rect::from_min_max(Pos2::new(0.0, 0.5 - f / 2.0), Pos2::new(1.0, 0.5 + f / 2.0))
    }
}

impl App {
    fn paint_backdrop(&self, painter: &Painter, rect: Rect) {
        let quad = [
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ];

        let draw = |idx: Option<i64>, alpha: f32| {
            let Some(i) = idx else { return };
            let Some(tex) = self.backdrops.get(&i) else {
                return;
            };
            let cover = Cover::new(rect.center(), rect.size(), tex.size_vec2());
            painter.add(geom::textured_polygon(
                &quad,
                tex.id(),
                &cover,
                theme::tint(1.0, alpha),
            ));
        };

        draw(self.prev_backdrop, 1.0);
        draw(self.shown_backdrop, self.fade);
    }

    fn paint_scrim(&self, painter: &Painter, rect: Rect) {
        let top = rect.height() * 0.19;
        painter.add(geom::vertical_gradient(
            Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), top)),
            Color32::from_black_alpha(196),
            Color32::TRANSPARENT,
        ));

        let bottom = rect.height() * 0.34;
        painter.add(geom::vertical_gradient(
            Rect::from_min_size(
                Pos2::new(rect.left(), rect.bottom() - bottom),
                Vec2::new(rect.width(), bottom),
            ),
            Color32::TRANSPARENT,
            Color32::from_black_alpha(214),
        ));

        // The grid puts light tiles across the whole screen, so it needs a
        // steadier ground than the slice view does.
        if self.view_mode == ViewMode::Grid && matches!(self.screen, Screen::Loaded) {
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(120));
        }
    }

    fn star_marker(&self, painter: &Painter, at: Pos2, alpha: f32) {
        painter.text(
            at,
            Align2::LEFT_BOTTOM,
            "★",
            FontId::new(13.0, self.fonts.mono.clone()),
            self.accent.gamma_multiply(alpha),
        );
    }

    fn paint_strip(&self, painter: &Painter, rect: Rect, m: &Metrics) {
        if self.order.is_empty() {
            return;
        }

        let top = rect.center().y - m.slice_h * 0.5;
        let offset = rect.center().x - self.centre_x(m);
        let near = self.pos.round() as i64;

        let mut slots: Vec<i64> = ((near - RENDER_WINDOW).max(0)
            ..=(near + RENDER_WINDOW).min(self.order.len() as i64 - 1))
            .collect();
        slots.sort_by(|a, b| {
            let da = (*a as f32 - self.pos).abs();
            let db = (*b as f32 - self.pos).abs();
            db.total_cmp(&da)
        });

        for slot in slots {
            let alpha = self.alpha_at(slot);
            if alpha <= 0.004 {
                continue;
            }
            let Some(&row) = self.order.get(slot as usize) else {
                continue;
            };

            let bump = self.bump(slot);
            let w = self.width_at(slot, m);
            let left = self.x_at(slot, m) + offset;
            let quad = geom::slice_quad(left, top, w, m.slice_h, m.skew);
            let outline = geom::rounded_outline(&quad, m.radius, 5);

            match self.slices.get(&row) {
                Some(tex) => {
                    let centre = Pos2::new(left + w * 0.5 + m.skew * 0.5, top + m.slice_h * 0.5);
                    let cover =
                        Cover::new(centre, Vec2::new(w + m.skew, m.slice_h), tex.size_vec2());
                    let brightness = 0.42 + 0.58 * bump;
                    painter.add(geom::textured_polygon(
                        &outline,
                        tex.id(),
                        &cover,
                        theme::tint(brightness, alpha),
                    ));
                }
                None => {
                    let base = if self.failed.contains(&row) {
                        Color32::from_rgb(0x2a, 0x18, 0x18)
                    } else if self.requested.contains(&row) {
                        theme::INK_600
                    } else {
                        theme::INK_700
                    };
                    painter.add(geom::solid_polygon(&outline, base.gamma_multiply(alpha)));
                }
            }

            painter.add(Shape::closed_line(
                outline.clone(),
                Stroke::new(1.0, Color32::from_white_alpha((22.0 * alpha) as u8)),
            ));

            if let Some(r) = self.rows.get(row as usize) {
                if self.library.is_favourite(&r.key) {
                    self.star_marker(
                        painter,
                        Pos2::new(left + 8.0, top + m.slice_h - 8.0),
                        alpha,
                    );
                }
            }

            if slot == self.cursor && bump > 0.01 {
                painter.add(Shape::closed_line(
                    outline,
                    Stroke::new(2.0, self.accent.gamma_multiply(bump)),
                ));
            }
        }
    }

    fn paint_grid(&self, painter: &Painter) {
        if self.order.is_empty() {
            return;
        }
        let view = self.grid_viewport();
        let cols = self.columns();
        let (tw, th) = self.tile_size(cols);

        let clip = painter.with_clip_rect(view);

        let first_row = ((self.grid_scroll / (th + GRID_GAP)).floor() as i64 - 1).max(0);
        let rows_visible = (view.height() / (th + GRID_GAP)).ceil() as i64 + 2;

        for gr in first_row..first_row + rows_visible {
            for gc in 0..cols {
                let slot = gr * cols + gc;
                if slot >= self.order.len() as i64 {
                    break;
                }
                let Some(&row) = self.order.get(slot as usize) else {
                    continue;
                };

                let tile = Rect::from_min_size(
                    Pos2::new(
                        view.left() + gc as f32 * (tw + GRID_GAP),
                        view.top() + gr as f32 * (th + GRID_GAP) - self.grid_scroll,
                    ),
                    Vec2::new(tw, th),
                );
                if !tile.intersects(view) {
                    continue;
                }

                let selected = slot == self.cursor;

                match self.slices.get(&row) {
                    Some(tex) => {
                        let uv = cover_uv(tex.size_vec2(), tile.size());
                        clip.image(
                            tex.id(),
                            tile,
                            uv,
                            theme::tint(if selected { 1.0 } else { 0.72 }, 1.0),
                        );
                    }
                    None => {
                        let base = if self.failed.contains(&row) {
                            Color32::from_rgb(0x2a, 0x18, 0x18)
                        } else {
                            theme::INK_700
                        };
                        clip.rect_filled(tile, 3.0, base);
                    }
                }

                if let Some(r) = self.rows.get(row as usize) {
                    if self.library.is_favourite(&r.key) {
                        clip.text(
                            Pos2::new(tile.left() + 7.0, tile.bottom() - 5.0),
                            Align2::LEFT_BOTTOM,
                            "★",
                            FontId::new(13.0, self.fonts.mono.clone()),
                            self.accent,
                        );
                    }
                }

                clip.rect_stroke(
                    tile,
                    3.0,
                    if selected {
                        Stroke::new(2.0, self.accent)
                    } else {
                        Stroke::new(1.0, Color32::from_white_alpha(24))
                    },
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

/* --------------------------------------------------------------- chrome -- */

/// A clickable toolbar chip. Returns true when clicked.
#[allow(clippy::too_many_arguments)]
fn chip(
    ui: &mut egui::Ui,
    painter: &Painter,
    rect: Rect,
    id: &str,
    label: &str,
    font: FontId,
    active: bool,
    accent: Color32,
) -> bool {
    let r = ui.interact(rect, egui::Id::new(id), Sense::click());
    let bg = if active {
        accent.gamma_multiply(0.24)
    } else if r.hovered() {
        Color32::from_white_alpha(20)
    } else {
        Color32::from_black_alpha(110)
    };
    painter.rect_filled(rect, 2.0, bg);
    if active {
        painter.rect_stroke(
            rect,
            2.0,
            Stroke::new(1.0, accent.gamma_multiply(0.75)),
            egui::StrokeKind::Inside,
        );
    }
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        font,
        if active { theme::TEXT } else { theme::DIM },
    );
    r.clicked()
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = theme::INK_900;
        [
            c.r() as f32 / 255.0,
            c.g() as f32 / 255.0,
            c.b() as f32 / 255.0,
            1.0,
        ]
    }

    /// Everything that isn't drawing. eframe runs this immediately before `ui`,
    /// and it is where the repaint request comes from — the loop is otherwise
    /// idle and burns nothing when nothing is moving.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.drain(ctx);
        self.handle_input(ctx);
        let moving = self.animate(ctx);
        self.sync_backdrop();
        self.pump(ctx);
        if moving {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mono = self.fonts.mono.clone();
        let display = self.fonts.display.clone();

        let rect = ui.max_rect();
        self.screen_rect = rect;
        let painter = ui.painter().clone();
        let m = Metrics::new(rect.size());
        let pad = self.pad();

        self.paint_backdrop(&painter, rect);
        self.paint_scrim(&painter, rect);

        if matches!(self.screen, Screen::Loaded) {
            match self.view_mode {
                ViewMode::Slices => self.paint_strip(&painter, rect, &m),
                ViewMode::Grid => self.paint_grid(&painter),
            }
        }

        // ---- top bar ---------------------------------------------------
        painter.text(
            Pos2::new(rect.left() + pad, rect.top() + 30.0),
            Align2::LEFT_CENTER,
            "G I T W A L L",
            FontId::new(11.0, mono.clone()),
            theme::FAINT,
        );

        let field = Rect::from_min_size(
            Pos2::new(rect.left() + pad + 130.0, rect.top() + 16.0),
            Vec2::new(460.0, 30.0),
        );
        let edit = ui.put(
            field,
            egui::TextEdit::singleline(&mut self.url)
                .id(repo_field())
                .font(FontId::new(13.0, mono.clone()))
                .hint_text("repo URL, imgur album, or search")
                .desired_width(f32::INFINITY),
        );
        if edit.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
            let url = self.url.clone();
            self.load(url);
        }

        if matches!(self.screen, Screen::Loaded) && self.source == Source::Repo {
            painter.text(
                Pos2::new(rect.right() - pad, rect.top() + 30.0),
                Align2::RIGHT_CENTER,
                format!(
                    "{} · {} images · {}",
                    self.commit_short,
                    self.repo_rows.len(),
                    human_bytes(self.total_bytes)
                ),
                FontId::new(11.0, mono.clone()),
                theme::FAINT,
            );
        }

        // ---- toolbar ---------------------------------------------------
        if matches!(self.screen, Screen::Loaded) {
            let font = FontId::new(11.0, mono.clone());
            let h = 26.0;
            let measure = |t: &str| {
                painter
                    .layout_no_wrap(t.to_owned(), font.clone(), theme::TEXT)
                    .size()
                    .x
            };

            // Build the segment list first so the whole bar can be centred.
            #[derive(Clone)]
            enum Seg {
                Btn(&'static str, String, bool),
                Swatches,
                Gap,
            }
            let mut segs: Vec<Seg> = vec![
                Seg::Btn("vm-slices", "▤".into(), self.view_mode == ViewMode::Slices),
                Seg::Btn("vm-grid", "▦".into(), self.view_mode == ViewMode::Grid),
                Seg::Gap,
                Seg::Btn("src-repo", "REPO".into(), self.source == Source::Repo),
                Seg::Btn(
                    "src-fav",
                    format!("★ {}", self.library.favourites().len()),
                    self.source == Source::Favourites,
                ),
            ];
            if self.source == Source::Repo {
                segs.push(Seg::Btn("only-fav", "STARRED".into(), self.only_favs));
            }
            if self.sections.len() > 1 {
                segs.push(Seg::Gap);
                segs.push(Seg::Btn(
                    "section",
                    self.filter_dir.clone().unwrap_or_else(|| "all".into()),
                    self.filter_dir.is_some(),
                ));
            }
            segs.push(Seg::Gap);
            segs.push(Seg::Btn("sort", "◐ HUE".into(), self.sort_colour));
            segs.push(Seg::Swatches);
            if self.filters_active() {
                segs.push(Seg::Btn("clear", "✕".into(), false));
            }

            let sw = 14.0;
            let swatch_w = (colour::HUES as f32 + 1.0) * (sw + 2.0);
            let total: f32 = segs
                .iter()
                .map(|s| match s {
                    Seg::Btn(_, t, _) => measure(t) + 20.0 + 4.0,
                    Seg::Swatches => swatch_w + 10.0,
                    Seg::Gap => 10.0,
                })
                .sum();

            let mut x = rect.center().x - total / 2.0;
            let y = rect.top() + 66.0;

            let mut clicked: Option<&'static str> = None;
            let mut swatch_clicked: Option<Swatch> = None;

            for seg in segs {
                match seg {
                    Seg::Gap => x += 10.0,
                    Seg::Btn(id, text, active) => {
                        let w = measure(&text) + 20.0;
                        let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h));
                        if chip(ui, &painter, r, id, &text, font.clone(), active, self.accent) {
                            clicked = Some(id);
                        }
                        x += w + 4.0;
                    }
                    Seg::Swatches => {
                        x += 5.0;
                        for s in Swatch::all() {
                            let r = Rect::from_min_size(Pos2::new(x, y + 6.0), Vec2::new(sw, h - 12.0));
                            let hit = ui.interact(
                                r.expand(2.0),
                                egui::Id::new(("sw", s.label())),
                                Sense::click(),
                            );
                            let [cr, cg, cb] = s.rgb();
                            let on = self.filter_swatch == Some(s);
                            let col = Color32::from_rgb(cr, cg, cb);
                            painter.rect_filled(
                                r,
                                1.0,
                                if on || hit.hovered() {
                                    col
                                } else {
                                    col.gamma_multiply(0.55)
                                },
                            );
                            if on {
                                painter.rect_stroke(
                                    r.expand(2.0),
                                    2.0,
                                    Stroke::new(1.0, theme::TEXT),
                                    egui::StrokeKind::Outside,
                                );
                            }
                            if hit.clicked() {
                                swatch_clicked = Some(s);
                            }
                            x += sw + 2.0;
                        }
                        x += 5.0;
                    }
                }
            }

            match clicked {
                Some("vm-slices") => {
                    self.view_mode = ViewMode::Slices;
                }
                Some("vm-grid") => {
                    self.view_mode = ViewMode::Grid;
                    self.ensure_grid_visible();
                }
                Some("src-repo") => self.set_source(Source::Repo),
                Some("src-fav") => {
                    if self.library.favourites().is_empty() {
                        self.say("No favourites yet — press F on a wallpaper", true, &ctx);
                    } else {
                        self.set_source(Source::Favourites);
                    }
                }
                Some("only-fav") => {
                    self.only_favs = !self.only_favs;
                    self.rebuild_order();
                }
                Some("section") => {
                    // Cycle: all -> each section -> all
                    let next = match &self.filter_dir {
                        None => self.sections.first().cloned(),
                        Some(cur) => {
                            let i = self.sections.iter().position(|s| s == cur).unwrap_or(0);
                            self.sections.get(i + 1).cloned()
                        }
                    };
                    self.filter_dir = next;
                    self.rebuild_order();
                }
                Some("sort") => {
                    self.sort_colour = !self.sort_colour;
                    self.rebuild_order();
                }
                Some("clear") => self.clear_filters(),
                _ => {}
            }
            if let Some(s) = swatch_clicked {
                self.filter_swatch = if self.filter_swatch == Some(s) {
                    None
                } else {
                    Some(s)
                };
                self.rebuild_order();
            }

            // Be honest that colour data only exists for fetched images.
            if self.sort_colour || self.filter_swatch.is_some() {
                let known = self.metas.len();
                painter.text(
                    Pos2::new(rect.center().x, y + h + 12.0),
                    Align2::CENTER_TOP,
                    format!(
                        "colour known for {known} of {} — scroll to load more",
                        self.rows.len()
                    ),
                    FontId::new(10.0, mono.clone()),
                    theme::FAINT,
                );
            }
        }

        // ---- centre states ---------------------------------------------
        match &self.screen {
            Screen::Empty => {
                let history: Vec<Visit> = self.library.history().to_vec();
                if history.is_empty() {
                    painter.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "Point it at a repo.",
                        FontId::new(64.0, display.clone()),
                        theme::TEXT,
                    );
                    painter.text(
                        rect.center() + Vec2::new(0.0, 48.0),
                        Align2::CENTER_CENTER,
                        "A GitHub repo, an Imgur album, or just type what you want.",
                        FontId::new(14.0, mono.clone()),
                        theme::DIM,
                    );
                } else {
                    let top = rect.center().y - (history.len() as f32 * 34.0) / 2.0 - 40.0;
                    painter.text(
                        Pos2::new(rect.center().x, top - 34.0),
                        Align2::CENTER_BOTTOM,
                        "Pick up where you left off",
                        FontId::new(30.0, display.clone()),
                        theme::TEXT,
                    );
                    let mut open: Option<String> = None;
                    let mut forget: Option<usize> = None;
                    for (i, v) in history.iter().enumerate() {
                        let r = Rect::from_center_size(
                            Pos2::new(rect.center().x, top + i as f32 * 34.0),
                            Vec2::new(560.0, 30.0),
                        );
                        let del = Rect::from_center_size(
                            Pos2::new(r.right() - 18.0, r.center().y),
                            Vec2::splat(22.0),
                        );
                        // Order matters: egui puts whatever is registered last
                        // on top, so the row has to go first for the delete
                        // target to receive the click at all.
                        let row_resp =
                            ui.interact(r, egui::Id::new(("hist", i)), Sense::click());
                        let del_resp =
                            ui.interact(del, egui::Id::new(("hist-del", i)), Sense::click());

                        let on = i == self.home_pick || row_resp.hovered();
                        if on {
                            painter.rect_filled(r, 2.0, Color32::from_white_alpha(16));
                        }
                        painter.text(
                            Pos2::new(r.left() + 14.0, r.center().y),
                            Align2::LEFT_CENTER,
                            v.label(),
                            FontId::new(13.0, mono.clone()),
                            if on { theme::TEXT } else { theme::DIM },
                        );
                        painter.text(
                            Pos2::new(del.left() - 12.0, r.center().y),
                            Align2::RIGHT_CENTER,
                            format!("{}", v.images),
                            FontId::new(11.0, mono.clone()),
                            theme::FAINT,
                        );
                        painter.text(
                            del.center(),
                            Align2::CENTER_CENTER,
                            "✕",
                            FontId::new(13.0, mono.clone()),
                            if del_resp.hovered() {
                                theme::DANGER
                            } else if on {
                                theme::FAINT
                            } else {
                                Color32::from_white_alpha(40)
                            },
                        );

                        if del_resp.clicked() {
                            forget = Some(i);
                        } else if row_resp.clicked() && !del_resp.hovered() {
                            open = Some(v.input.clone());
                        }
                    }

                    if let Some(i) = forget {
                        self.library.forget(i);
                        let _ = self.library.save();
                        self.home_pick = self
                            .home_pick
                            .min(self.library.history().len().saturating_sub(1));
                    } else if let Some(url) = open {
                        self.url = url.clone();
                        self.load(url);
                    }
                }
            }
            Screen::Working => {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "Reading the tree…",
                    FontId::new(14.0, mono.clone()),
                    theme::DIM,
                );
            }
            Screen::Fault(msg) => {
                painter.text(
                    rect.center() - Vec2::new(0.0, 24.0),
                    Align2::CENTER_CENTER,
                    "That didn't load.",
                    FontId::new(40.0, display.clone()),
                    theme::TEXT,
                );
                painter.text(
                    rect.center() + Vec2::new(0.0, 18.0),
                    Align2::CENTER_CENTER,
                    msg,
                    FontId::new(12.0, mono.clone()),
                    theme::DIM,
                );
            }
            Screen::Loaded => {
                if self.order.is_empty() {
                    painter.text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "Nothing matches those filters.",
                        FontId::new(30.0, display.clone()),
                        theme::DIM,
                    );
                } else if let Some(row) = self.row_at(self.cursor).cloned() {
                    let base = rect.bottom() - 76.0;
                    let starred = self.library.is_favourite(&row.key);

                    painter.text(
                        Pos2::new(rect.left() + pad, base),
                        Align2::LEFT_BOTTOM,
                        if starred {
                            format!("★ {}", row.name)
                        } else {
                            row.name.clone()
                        },
                        FontId::new((rect.width() * 0.024).clamp(30.0, 58.0), display.clone()),
                        theme::TEXT,
                    );

                    // Imgur reports dimensions in the album JSON, so they show
                    // immediately; GitHub only reveals them once the thumbnail
                    // has been decoded.
                    let dims = if row.width > 0 && row.height > 0 {
                        format!("{}×{} · ", row.width, row.height)
                    } else {
                        self.focused_row()
                            .and_then(|r| self.metas.get(&r))
                            .map(|m| format!("{}×{} · ", m.src_w, m.src_h))
                            .unwrap_or_default()
                    };
                    let origin = if self.source == Source::Favourites {
                        format!("{} · ", row.origin)
                    } else if row.group.is_empty() {
                        String::new()
                    } else {
                        format!("{}/ · ", row.group)
                    };
                    painter.text(
                        Pos2::new(rect.left() + pad, base + 18.0),
                        Align2::LEFT_BOTTOM,
                        format!("{origin}{dims}{} · {}", human_bytes(row.size), row.ext),
                        FontId::new(11.5, mono.clone()),
                        theme::FAINT,
                    );

                    let btn = Rect::from_min_size(
                        Pos2::new(rect.right() - pad - 190.0, base - 44.0),
                        Vec2::new(190.0, 44.0),
                    );
                    let label = if self.applying {
                        "SETTING…"
                    } else {
                        "SET WALLPAPER"
                    };
                    if chip(
                        ui,
                        &painter,
                        btn,
                        "apply",
                        label,
                        FontId::new(12.0, mono.clone()),
                        true,
                        self.accent,
                    ) {
                        self.apply();
                    }

                    painter.text(
                        Pos2::new(rect.right() - pad, base + 16.0),
                        Align2::RIGHT_BOTTOM,
                        "← → browse   ⏎ set   F star   G grid   ⇥ favourites   esc close",
                        FontId::new(10.5, mono.clone()),
                        theme::FAINT,
                    );

                    let y = rect.bottom() - 34.0;
                    let track = Rect::from_min_size(
                        Pos2::new(rect.left() + pad, y),
                        Vec2::new(rect.width() - pad * 2.0 - 130.0, 1.0),
                    );
                    painter.rect_filled(track, 0.0, Color32::from_white_alpha(20));
                    let frac = (self.pos + 1.0) / self.order.len() as f32;
                    painter.rect_filled(
                        Rect::from_min_size(
                            track.min,
                            Vec2::new(track.width() * frac.clamp(0.0, 1.0), 1.0),
                        ),
                        0.0,
                        self.accent,
                    );
                    painter.text(
                        Pos2::new(rect.right() - pad, y + 1.0),
                        Align2::RIGHT_CENTER,
                        format!("{} / {}", self.cursor + 1, self.order.len()),
                        FontId::new(11.0, mono.clone()),
                        theme::FAINT,
                    );
                }
            }
        }

        // ---- toast ------------------------------------------------------
        if let Some((msg, until, bad)) = self.toast.clone() {
            let now = ctx.input(|i| i.time);
            if now < until {
                let colour = if bad { theme::BAD } else { theme::TEXT };
                let at = Pos2::new(rect.center().x, rect.bottom() - rect.height() * 0.12);
                let galley =
                    painter.layout_no_wrap(msg.clone(), FontId::new(12.0, mono.clone()), colour);
                let box_rect =
                    Rect::from_center_size(at, galley.size() + Vec2::new(40.0, 22.0));
                painter.rect_filled(box_rect, 2.0, Color32::from_black_alpha(238));
                painter.galley(at - galley.size() * 0.5, galley, colour);
                ctx.request_repaint_after(Duration::from_millis(100));
            } else {
                self.toast = None;
            }
        }
    }
}
