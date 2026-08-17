#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Thin command layer over `gitwall-core`.
//!
//! Deliberately holds no logic beyond shaping data for the webview — anything
//! worth testing belongs in the core crate.

use std::sync::Arc;

use gitwall_core::{source::GithubClient, wallpaper, Backend, Cache, Fetcher, Listing, RepoRef};
use serde::Serialize;
use tauri::{Manager, State};
use tokio::sync::RwLock;

struct AppState {
    cache: Cache,
    fetcher: Fetcher,
    http: reqwest::Client,
    /// The repo currently being browsed. `Arc` so commands can take a cheap
    /// snapshot instead of holding the lock across an await.
    listing: RwLock<Option<Arc<Listing>>>,
    /// Repo passed on the command line, opened as soon as the window is up.
    initial: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageDto {
    name: String,
    path: String,
    /// Directory the image sits in, or "" at the repo root.
    dir: String,
    size: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RepoDto {
    display: String,
    owner: String,
    repo: String,
    commit: String,
    short_commit: String,
    truncated: bool,
    /// Sum of full-resolution bytes, so the UI can be honest about what a full
    /// browse would cost.
    total_bytes: u64,
    images: Vec<ImageDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThumbDto {
    index: usize,
    /// Absolute path; the frontend runs it through `convertFileSrc`.
    file: String,
    width: u32,
    height: u32,
    /// `#rrggbb`, ready to drop into CSS.
    accent: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendDto {
    name: String,
    available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendInfoDto {
    detected: Option<String>,
    problem: Option<String>,
    backends: Vec<BackendDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheDto {
    thumb_count: u64,
    thumb_bytes: u64,
    full_count: u64,
    full_bytes: u64,
}

fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// Resolve a repo reference and make it the active gallery.
#[tauri::command]
async fn resolve_repo(url: String, state: State<'_, AppState>) -> Result<RepoDto, String> {
    let repo = RepoRef::parse(&url).map_err(|e| e.to_string())?;

    let listing = match state.cache.load_listing(&repo) {
        Some(l) => l,
        None => {
            let l = GithubClient::new(state.http.clone())
                .resolve(&repo)
                .await
                .map_err(|e| e.to_string())?;
            // A failed cache write shouldn't block browsing.
            let _ = state.cache.save_listing(&l);
            l
        }
    };

    let dto = RepoDto {
        display: listing.repo.display(),
        owner: listing.repo.owner.clone(),
        repo: listing.repo.repo.clone(),
        short_commit: listing.commit.chars().take(7).collect(),
        commit: listing.commit.clone(),
        truncated: listing.truncated,
        total_bytes: listing.images.iter().map(|i| i.size).sum(),
        images: listing
            .images
            .iter()
            .map(|i| ImageDto {
                name: i.name.clone(),
                path: i.path.clone(),
                dir: i.path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default(),
                size: i.size,
            })
            .collect(),
    };

    *state.listing.write().await = Some(Arc::new(listing));
    Ok(dto)
}

/// Fetch (or read from cache) the thumbnail for one image.
#[tauri::command]
async fn load_thumb(index: usize, state: State<'_, AppState>) -> Result<ThumbDto, String> {
    let listing = state
        .listing
        .read()
        .await
        .clone()
        .ok_or("no repo loaded yet")?;

    let entry = listing
        .images
        .get(index)
        .ok_or_else(|| format!("image {index} is out of range"))?;

    let (path, meta) = state
        .fetcher
        .thumb(&state.cache, &listing, entry)
        .await
        .map_err(|e| e.to_string())?;

    Ok(ThumbDto {
        index,
        file: path.to_string_lossy().to_string(),
        width: meta.src_w,
        height: meta.src_h,
        accent: hex(meta.accent),
    })
}

/// Download the full-resolution image, without applying it.
///
/// Used for the full-screen backdrop once the user stops scrolling, so the
/// preview behind the strip is the real wallpaper rather than an upscaled
/// thumbnail. Cached, so coming back to an image costs nothing.
#[tauri::command]
async fn load_full(index: usize, state: State<'_, AppState>) -> Result<String, String> {
    let listing = state
        .listing
        .read()
        .await
        .clone()
        .ok_or("no repo loaded yet")?;

    let entry = listing
        .images
        .get(index)
        .ok_or_else(|| format!("image {index} is out of range"))?;

    let path = state
        .fetcher
        .full(&state.cache, &listing, entry)
        .await
        .map_err(|e| e.to_string())?;

    Ok(path.to_string_lossy().to_string())
}

/// Download the full-resolution image and hand it to the desktop.
#[tauri::command]
async fn apply_wallpaper(
    index: usize,
    backend: Option<String>,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let listing = state
        .listing
        .read()
        .await
        .clone()
        .ok_or("no repo loaded yet")?;

    let entry = listing
        .images
        .get(index)
        .ok_or_else(|| format!("image {index} is out of range"))?;

    let backend = match backend {
        Some(name) => Backend::from_name(&name).ok_or(format!("unknown backend `{name}`"))?,
        None => Backend::detect().map_err(|e| e.to_string())?,
    };

    let full = state
        .fetcher
        .full(&state.cache, &listing, entry)
        .await
        .map_err(|e| e.to_string())?;

    let applied = wallpaper::set_wallpaper(&state.cache, backend, &full, &entry.sha)
        .await
        .map_err(|e| e.to_string())?;

    Ok(applied.to_string_lossy().to_string())
}

/// The repo given on the command line, if any, so the picker can open straight
/// into it: `gitwall github.com/owner/repo`.
#[tauri::command]
fn initial_repo(state: State<'_, AppState>) -> Option<String> {
    state.initial.clone()
}

/// Set `GITWALL_PERF=1` to have the picker stress-scroll itself on startup and
/// report frame times. Development instrumentation, off by default.
#[tauri::command]
fn perf_mode() -> bool {
    std::env::var_os("GITWALL_PERF").is_some()
}

/// Frame-time results from the perf probe, printed where the shell can read
/// them (webview console output doesn't reliably reach stderr).
#[tauri::command]
fn report_perf(summary: String) {
    println!("PERF {summary}");
}

#[tauri::command]
fn backend_info() -> BackendInfoDto {
    let (detected, problem) = match Backend::detect() {
        Ok(b) => (Some(b.name().to_string()), None),
        Err(e) => (None, Some(e.to_string())),
    };

    BackendInfoDto {
        detected,
        problem,
        backends: Backend::survey()
            .into_iter()
            .map(|(b, available)| BackendDto {
                name: b.name().to_string(),
                available,
            })
            .collect(),
    }
}

#[tauri::command]
fn cache_stats(state: State<'_, AppState>) -> CacheDto {
    let s = state.cache.stats();
    CacheDto {
        thumb_count: s.thumb_count,
        thumb_bytes: s.thumb_bytes,
        full_count: s.full_count,
        full_bytes: s.full_bytes,
    }
}

#[tauri::command]
fn clear_cache(state: State<'_, AppState>) -> Result<(), String> {
    state.cache.clear().map_err(|e| e.to_string())
}

/// WebKitGTK's DMABUF renderer fails on NVIDIA drivers — it logs
/// "Failed to create GBM buffer of size WxH: Invalid argument" and paints
/// nothing, leaving a blank window. Disabling that renderer is the standard
/// workaround.
///
/// Applied only when an NVIDIA driver is actually loaded, so machines that
/// don't need it keep the faster path, and never over an explicit setting.
fn apply_nvidia_webkit_workaround() {
    // Escape hatch for measuring what the workaround actually costs.
    if std::env::var_os("GITWALL_FORCE_DMABUF").is_some() {
        return;
    }
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some() {
        return;
    }
    if std::path::Path::new("/proc/driver/nvidia/version").exists() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

fn main() {
    // Must happen before anything touches GTK or WebKit.
    apply_nvidia_webkit_workaround();

    let cache = match Cache::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gitwall: {e}");
            std::process::exit(1);
        }
    };

    let http = reqwest::Client::builder()
        .user_agent(concat!("gitwall/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("failed to build the http client");

    let state = AppState {
        cache,
        fetcher: Fetcher::new(http.clone()),
        http,
        listing: RwLock::new(None),
        initial: std::env::args().nth(1).filter(|a| !a.starts_with('-')),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            resolve_repo,
            load_thumb,
            load_full,
            apply_wallpaper,
            initial_repo,
            perf_mode,
            report_perf,
            backend_info,
            cache_stats,
            clear_cache
        ])
        .setup(|app| {
            if let Some(w) = app.get_webview_window("main") {
                // The picker is fullscreen by default, which is unhelpful while
                // working on it — this drops it into a normal window.
                if std::env::var_os("GITWALL_WINDOWED").is_some() {
                    let _ = w.set_fullscreen(false);
                    let _ = w.set_decorations(true);
                }
                let _ = w.set_focus();
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start gitwall");
}
