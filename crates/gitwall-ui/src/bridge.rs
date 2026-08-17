//! Async bridge between the render loop and `gitwall-core`.
//!
//! The UI thread never blocks and never touches the network or the image
//! decoder. A tokio runtime on its own thread owns all of that, decodes on the
//! blocking pool, and posts finished pixel buffers back over a channel, waking
//! the render loop with `request_repaint`.
//!
//! Image commands carry their own URLs rather than an index into some current
//! listing. That is what lets the favourites view — whose entries come from
//! many different repos — go down exactly the same path as a normal repo.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use egui::ColorImage;
use gitwall_core::source::GithubClient;
use gitwall_core::{wallpaper, Backend, Cache, Fetcher, RepoRef, ThumbMeta};

/// Backdrop source width. The backdrop is drawn across the whole screen, so it
/// gets upscaled hard — GPU bilinear filtering then *is* the blur, which is why
/// there is no per-frame blur pass anywhere in this app.
const BACKDROP_MAX: u32 = 420;

/// Cap for the full-resolution slice texture. The focused slice is only ever
/// ~1000px wide on screen, so uploading a 4K texture would be waste.
const SLICE_MAX: u32 = 1400;

/// Everything needed to fetch one image, from any source.
#[derive(Clone, Debug)]
pub struct Target {
    /// Cache key — git blob sha, or the Imgur id.
    pub key: String,
    /// Repo path or Imgur id; also the label in error messages.
    pub label: String,
    pub urls: [String; 2],
}

pub enum Cmd {
    Resolve(String),
    Thumb { row: usize, target: Target },
    Full { row: usize, target: Target },
    Apply { target: Target, name: String },
}

/// One wallpaper, from a GitHub repo, an Imgur album, or the favourites file.
///
/// Carries its own URLs rather than source-specific coordinates, so the UI
/// never has to know where a wallpaper came from.
#[derive(Clone, Debug)]
pub struct Row {
    pub key: String,
    pub name: String,
    pub label: String,
    /// Section (repo subdirectory). Empty when the source has no sections.
    pub group: String,
    pub ext: String,
    pub size: u64,
    /// 0 when unknown. GitHub only reveals dimensions after thumbnailing;
    /// Imgur reports them up front.
    pub width: u32,
    pub height: u32,
    pub origin: String,
    /// Browsing-sized image. Imgur serves a real thumbnail here (~48 KB for a
    /// 4K wallpaper); GitHub only has the original.
    pub preview: [String; 2],
    pub full: [String; 2],
}

impl Row {
    pub fn preview_target(&self) -> Target {
        Target {
            key: self.key.clone(),
            label: self.label.clone(),
            urls: self.preview.clone(),
        }
    }

    pub fn full_target(&self) -> Target {
        Target {
            key: self.key.clone(),
            label: self.label.clone(),
            urls: self.full.clone(),
        }
    }

    pub fn favourite(&self) -> gitwall_core::Favourite {
        gitwall_core::Favourite {
            key: self.key.clone(),
            name: self.name.clone(),
            label: self.label.clone(),
            group: self.group.clone(),
            ext: self.ext.clone(),
            size: self.size,
            width: self.width,
            height: self.height,
            origin: self.origin.clone(),
            preview: self.preview.clone(),
            full: self.full.clone(),
            added: gitwall_core::library::now(),
        }
    }

    pub fn from_favourite(f: &gitwall_core::Favourite) -> Self {
        Self {
            key: f.key.clone(),
            name: f.name.clone(),
            label: f.label.clone(),
            group: f.group.clone(),
            ext: f.ext.clone(),
            size: f.size,
            width: f.width,
            height: f.height,
            origin: f.origin.clone(),
            preview: f.preview.clone(),
            full: f.full.clone(),
        }
    }
}

pub enum Evt {
    Resolved {
        /// Human label for the collection.
        title: String,
        /// Small print for the top-right corner: pinned commit, album hash.
        badge: String,
        /// Exactly what the user typed, for the history entry.
        input: String,
        total_bytes: u64,
        truncated: bool,
        rows: Vec<Row>,
    },
    ResolveFailed(String),
    Thumb {
        row: usize,
        slice: Arc<ColorImage>,
        backdrop: Arc<ColorImage>,
        meta: ThumbMeta,
    },
    ThumbFailed {
        row: usize,
    },
    /// Sharper slice and backdrop, decoded from the full-resolution original
    /// once the user settled on this image.
    Full {
        row: usize,
        slice: Arc<ColorImage>,
        backdrop: Arc<ColorImage>,
    },
    Applied(String),
    ApplyFailed(String),
}

pub struct Bridge {
    tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    rx: Receiver<Evt>,
}

impl Bridge {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn try_recv(&self) -> Option<Evt> {
        self.rx.try_recv().ok()
    }
}

struct Core {
    cache: Cache,
    fetcher: Fetcher,
    http: reqwest::Client,
}

pub fn spawn(ctx: egui::Context, cache: Cache) -> Bridge {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
    let (evt_tx, evt_rx) = channel::<Evt>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to start the async runtime");

        rt.block_on(async move {
            let http = reqwest::Client::builder()
                .user_agent(concat!("gitwall/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("failed to build the http client");

            let core = Arc::new(Core {
                cache,
                fetcher: Fetcher::new(http.clone()),
                http,
            });

            while let Some(cmd) = cmd_rx.recv().await {
                let core = core.clone();
                let evt_tx = evt_tx.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Some(evt) = handle(cmd, core).await {
                        let _ = evt_tx.send(evt);
                        // Wake the render loop; it is otherwise idle.
                        ctx.request_repaint();
                    }
                });
            }
        });
    });

    Bridge { tx: cmd_tx, rx: evt_rx }
}

fn to_color_image(img: &image::RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied(
        [img.width() as usize, img.height() as usize],
        img.as_raw(),
    )
}

fn backdrop_of(img: &image::DynamicImage) -> ColorImage {
    let small = img.thumbnail(BACKDROP_MAX, BACKDROP_MAX);
    // A light gaussian first, so upscaling reads as a smooth blur rather than
    // blocky bilinear interpolation.
    to_color_image(&image::imageops::blur(&small.to_rgba8(), 1.6))
}

fn slice_of(img: &image::DynamicImage) -> ColorImage {
    let fitted = if img.width() > SLICE_MAX {
        img.thumbnail(SLICE_MAX, SLICE_MAX)
    } else {
        img.clone()
    };
    to_color_image(&fitted.to_rgba8())
}

async fn handle(cmd: Cmd, core: Arc<Core>) -> Option<Evt> {
    match cmd {
        Cmd::Resolve(url) => Some(resolve(url, core).await),

        Cmd::Thumb { row, target } => {
            match core
                .fetcher
                .thumb_at(&core.cache, &target.key, &target.label, target.urls)
                .await
            {
                Ok((path, meta)) => {
                    let decoded = tokio::task::spawn_blocking(move || {
                        let img = image::open(&path).ok()?;
                        Some((to_color_image(&img.to_rgba8()), backdrop_of(&img)))
                    })
                    .await
                    .ok()
                    .flatten();

                    match decoded {
                        Some((slice, backdrop)) => Some(Evt::Thumb {
                            row,
                            slice: Arc::new(slice),
                            backdrop: Arc::new(backdrop),
                            meta,
                        }),
                        None => Some(Evt::ThumbFailed { row }),
                    }
                }
                Err(_) => Some(Evt::ThumbFailed { row }),
            }
        }

        Cmd::Full { row, target } => {
            let path = core
                .fetcher
                .full_at(&core.cache, &target.key, &target.label, target.urls)
                .await
                .ok()?;

            let decoded = tokio::task::spawn_blocking(move || {
                let img = image::open(&path).ok()?;
                Some((slice_of(&img), backdrop_of(&img)))
            })
            .await
            .ok()
            .flatten()?;

            Some(Evt::Full {
                row,
                slice: Arc::new(decoded.0),
                backdrop: Arc::new(decoded.1),
            })
        }

        Cmd::Apply { target, name } => {
            let backend = match Backend::detect() {
                Ok(b) => b,
                Err(e) => return Some(Evt::ApplyFailed(e.to_string())),
            };

            let full = match core
                .fetcher
                .full_at(&core.cache, &target.key, &target.label, target.urls)
                .await
            {
                Ok(p) => p,
                Err(e) => return Some(Evt::ApplyFailed(e.to_string())),
            };

            match wallpaper::set_wallpaper(&core.cache, backend, &full, &target.key).await {
                Ok(_) => Some(Evt::Applied(name)),
                Err(e) => Some(Evt::ApplyFailed(e.to_string())),
            }
        }
    }
}

/// Pick a source from the URL and turn it into rows.
async fn resolve(url: String, core: Arc<Core>) -> Evt {
    // Imgur first: its URLs are unambiguous, whereas `RepoRef::parse` is
    // permissive enough to mangle one into an "owner/repo".
    if let Some(reference) = gitwall_core::imgur::parse_ref(&url) {
        return resolve_imgur(url, reference, core).await;
    }
    // Last: anything that isn't a URL or an owner/repo is a search phrase.
    if gitwall_core::wallhaven::looks_like_query(&url) {
        return resolve_search(url, core).await;
    }
    resolve_github(url, core).await
}

async fn resolve_search(query: String, core: Arc<Core>) -> Evt {
    let found = match gitwall_core::wallhaven::WallhavenClient::new(core.http.clone())
        .search(query.trim())
        .await
    {
        Ok(s) => s,
        Err(e) => return Evt::ResolveFailed(e.to_string()),
    };

    let rows = found
        .hits
        .iter()
        .map(|h| Row {
            key: h.id.clone(),
            name: h.id.clone(),
            label: h.id.clone(),
            group: String::new(),
            ext: h.ext.to_ascii_uppercase(),
            size: h.size,
            width: h.width,
            height: h.height,
            origin: "wallhaven".into(),
            // Native thumbnail, with the original as the fallback.
            preview: [h.thumb.clone(), h.full.clone()],
            full: [h.full.clone(), h.full.clone()],
        })
        .collect::<Vec<_>>();

    Evt::Resolved {
        title: found.query.clone(),
        badge: format!("wallhaven · {} of {} results", rows.len(), found.total),
        input: found.query,
        total_bytes: found.hits.iter().map(|h| h.size).sum(),
        // Not truncation — a search is paged, and the badge already says how
        // many of how many were fetched.
        truncated: false,
        rows,
    }
}

async fn resolve_imgur(
    input: String,
    reference: gitwall_core::imgur::ImgurRef,
    core: Arc<Core>,
) -> Evt {
    let album = match gitwall_core::imgur::ImgurClient::new(core.http.clone())
        .resolve(&reference)
        .await
    {
        Ok(a) => a,
        Err(e) => return Evt::ResolveFailed(e.to_string()),
    };

    let origin = format!("imgur/{}", album.hash);
    let rows = album
        .items
        .iter()
        .map(|it| Row {
            key: it.id.clone(),
            name: it.name.clone(),
            label: it.id.clone(),
            // Imgur albums are flat — no sections to filter by.
            group: String::new(),
            ext: it.ext.to_ascii_uppercase(),
            size: it.size,
            width: it.width,
            height: it.height,
            origin: origin.clone(),
            // Native thumbnail, with the original as fallback if it 404s.
            preview: [it.preview_url(), it.full_url()],
            full: [it.full_url(), it.full_url()],
        })
        .collect();

    Evt::Resolved {
        title: album.title.clone(),
        badge: origin,
        input,
        total_bytes: album.total_bytes(),
        truncated: false,
        rows,
    }
}

async fn resolve_github(url: String, core: Arc<Core>) -> Evt {
    let repo = match RepoRef::parse(&url) {
        Ok(r) => r,
        Err(e) => return Evt::ResolveFailed(e.to_string()),
    };

    let listing = match core.cache.load_listing(&repo) {
        Some(l) => l,
        None => match GithubClient::new(core.http.clone()).resolve(&repo).await {
            Ok(l) => {
                // A failed cache write must not block browsing.
                let _ = core.cache.save_listing(&l);
                l
            }
            Err(e) => return Evt::ResolveFailed(e.to_string()),
        },
    };

    let origin = format!("{}/{}", listing.repo.owner, listing.repo.repo);
    let rows: Vec<Row> = listing
        .images
        .iter()
        .map(|i| {
            let urls = listing.urls(i);
            Row {
                key: i.sha.clone(),
                name: i.name.clone(),
                label: i.path.clone(),
                group: i
                    .path
                    .rsplit_once('/')
                    .map(|(d, _)| d.to_string())
                    .unwrap_or_default(),
                ext: i
                    .path
                    .rsplit_once('.')
                    .map(|(_, e)| e.to_ascii_uppercase())
                    .unwrap_or_default(),
                size: i.size,
                // GitHub does not report dimensions; they arrive with the
                // thumbnail instead.
                width: 0,
                height: 0,
                origin: origin.clone(),
                // No thumbnail service — the original is the only option.
                preview: urls.clone(),
                full: urls,
            }
        })
        .collect();

    Evt::Resolved {
        title: match &listing.repo.subdir {
            Some(d) => format!("{origin} · {d}"),
            None => origin,
        },
        badge: listing.commit.chars().take(7).collect(),
        input: url,
        total_bytes: listing.images.iter().map(|i| i.size).sum(),
        truncated: listing.truncated,
        rows,
    }
}
