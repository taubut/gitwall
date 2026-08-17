//! Detecting the desktop and actually setting the wallpaper.
//!
//! There is no common Wayland protocol for this, so every compositor needs its
//! own poke. Detection is by environment, with `GITWALL_BACKEND` as an escape
//! hatch when the guess is wrong.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::cache::Cache;
use crate::{Error, Result};

/// How many previously-applied wallpapers to keep on disk.
const KEEP_APPLIED: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// KDE Plasma 5.18+ / 6
    Plasma,
    /// GNOME and forks, via gsettings
    Gnome,
    /// wlroots compositors (sway, Hyprland, river) — animated, preferred
    Swww,
    /// Hyprland's own daemon, if swww isn't installed
    Hyprpaper,
    /// Plain sway fallback
    Swaybg,
    /// X11
    Feh,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Plasma => "plasma",
            Backend::Gnome => "gnome",
            Backend::Swww => "swww",
            Backend::Hyprpaper => "hyprpaper",
            Backend::Swaybg => "swaybg",
            Backend::Feh => "feh",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "plasma" | "kde" => Backend::Plasma,
            "gnome" => Backend::Gnome,
            "swww" => Backend::Swww,
            "hyprpaper" => Backend::Hyprpaper,
            "swaybg" => Backend::Swaybg,
            "feh" => Backend::Feh,
            _ => return None,
        })
    }

    /// The binary this backend drives.
    fn binary(self) -> &'static str {
        match self {
            Backend::Plasma => "plasma-apply-wallpaperimage",
            Backend::Gnome => "gsettings",
            Backend::Swww => "swww",
            Backend::Hyprpaper => "hyprctl",
            Backend::Swaybg => "swaybg",
            Backend::Feh => "feh",
        }
    }

    pub fn is_available(self) -> bool {
        which(self.binary())
    }

    /// Pick a backend for the current session.
    ///
    /// Ordered most-specific first: a session that is both "wayland" and
    /// "Plasma" should get the Plasma path, not the generic wlroots one.
    pub fn detect() -> Result<Self> {
        if let Ok(forced) = std::env::var("GITWALL_BACKEND") {
            let b = Backend::from_name(&forced).ok_or_else(|| {
                Error::NoBackend(format!("GITWALL_BACKEND=`{forced}` is not a known backend"))
            })?;
            if !b.is_available() {
                return Err(Error::NoBackend(format!(
                    "GITWALL_BACKEND=`{forced}` but `{}` is not on PATH",
                    b.binary()
                )));
            }
            return Ok(b);
        }

        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .or_else(|_| std::env::var("XDG_SESSION_DESKTOP"))
            .unwrap_or_default()
            .to_ascii_uppercase();

        let mut candidates: Vec<Backend> = Vec::new();

        if desktop.contains("KDE") || desktop.contains("PLASMA") {
            candidates.push(Backend::Plasma);
        }
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() || desktop.contains("HYPRLAND")
        {
            candidates.extend([Backend::Swww, Backend::Hyprpaper]);
        }
        if std::env::var_os("SWAYSOCK").is_some() || desktop.contains("SWAY") {
            candidates.extend([Backend::Swww, Backend::Swaybg]);
        }
        if desktop.contains("GNOME") {
            candidates.push(Backend::Gnome);
        }
        // Generic fallbacks by session type.
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            candidates.extend([Backend::Swww, Backend::Swaybg]);
        }
        if std::env::var_os("DISPLAY").is_some() {
            candidates.push(Backend::Feh);
        }

        if let Some(b) = candidates.iter().copied().find(|b| b.is_available()) {
            return Ok(b);
        }

        let wanted: Vec<&str> = candidates.iter().map(|b| b.binary()).collect();
        Err(Error::NoBackend(if wanted.is_empty() {
            "could not identify the desktop session. Set GITWALL_BACKEND to one of: \
             plasma, gnome, swww, hyprpaper, swaybg, feh."
                .to_string()
        } else {
            format!(
                "detected this session but none of its tools are installed. Install one of: {}.",
                wanted.join(", ")
            )
        }))
    }

    /// List every backend with whether its tool is present. Drives the UI's
    /// settings panel.
    pub fn survey() -> Vec<(Backend, bool)> {
        [
            Backend::Plasma,
            Backend::Gnome,
            Backend::Swww,
            Backend::Hyprpaper,
            Backend::Swaybg,
            Backend::Feh,
        ]
        .into_iter()
        .map(|b| (b, b.is_available()))
        .collect()
    }

    pub async fn apply(self, path: &Path) -> Result<()> {
        let p = path.to_string_lossy().to_string();

        match self {
            Backend::Plasma => run(self, "plasma-apply-wallpaperimage", &[&p]).await,

            Backend::Gnome => {
                let uri = format!("file://{p}");
                // Set both so the wallpaper follows the light/dark switch.
                for key in ["picture-uri", "picture-uri-dark"] {
                    run(
                        self,
                        "gsettings",
                        &["set", "org.gnome.desktop.background", key, &uri],
                    )
                    .await?;
                }
                run(
                    self,
                    "gsettings",
                    &["set", "org.gnome.desktop.background", "picture-options", "zoom"],
                )
                .await
            }

            Backend::Swww => {
                self.ensure_swww_daemon().await?;
                run(
                    self,
                    "swww",
                    &["img", &p, "--transition-type", "fade", "--transition-duration", "1"],
                )
                .await
            }

            Backend::Hyprpaper => {
                run(self, "hyprctl", &["hyprpaper", "preload", &p]).await?;
                run(self, "hyprctl", &["hyprpaper", "wallpaper", &format!(",{p}")]).await
            }

            Backend::Swaybg => {
                // swaybg has no IPC — the only way to change the image is to
                // replace the process.
                let _ = Command::new("pkill").args(["-x", "swaybg"]).status().await;
                Command::new("swaybg")
                    .args(["-i", &p, "-m", "fill"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| Error::BackendFailed {
                        backend: self.name().into(),
                        detail: format!("could not spawn swaybg: {e}"),
                    })?;
                Ok(())
            }

            Backend::Feh => run(self, "feh", &["--no-fehbg", "--bg-fill", &p]).await,
        }
    }

    /// `swww img` fails if the daemon isn't up, so start it on demand.
    async fn ensure_swww_daemon(self) -> Result<()> {
        let up = Command::new("swww")
            .arg("query")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);

        if up {
            return Ok(());
        }

        let daemon = if which("swww-daemon") { "swww-daemon" } else { "swww" };
        let mut cmd = Command::new(daemon);
        if daemon == "swww" {
            cmd.arg("init");
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::BackendFailed {
                backend: self.name().into(),
                detail: format!("could not start the swww daemon: {e}"),
            })?;

        // Give it a moment to take the wayland layer-shell surface.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let ok = Command::new("swww")
                .arg("query")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return Ok(());
            }
        }

        Err(Error::BackendFailed {
            backend: self.name().into(),
            detail: "swww daemon did not come up within 2s".into(),
        })
    }
}

async fn run(backend: Backend, bin: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| Error::BackendFailed {
            backend: backend.name().into(),
            detail: format!("could not run `{bin}`: {e}"),
        })?;

    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(Error::BackendFailed {
        backend: backend.name().into(),
        detail: format!(
            "`{bin} {}` exited {}: {}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            if stderr.is_empty() { "(no output)" } else { &stderr }
        ),
    })
}

fn which(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file() && is_executable(&candidate)
    })
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

/// Copy the chosen image somewhere durable, then apply it.
///
/// The copy matters: pointing the desktop straight at a cache file means
/// `gitwall --clear-cache` (or a tmp reaper) would blank the wallpaper.
pub async fn set_wallpaper(
    cache: &Cache,
    backend: Backend,
    source: &Path,
    sha: &str,
) -> Result<PathBuf> {
    let ext = source
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "img".into());

    let dest = cache.wallpaper_dir().join(format!("{sha}.{ext}"));

    if !dest.exists() {
        std::fs::copy(source, &dest).map_err(|e| Error::io(&dest, e))?;
    }

    backend.apply(&dest).await?;
    prune_applied(&cache.wallpaper_dir(), &dest);
    Ok(dest)
}

/// Keep the directory from growing forever, but never touch the file we just
/// applied.
fn prune_applied(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            Some((md.modified().ok()?, e.path()))
        })
        .collect();

    if files.len() <= KEEP_APPLIED {
        return;
    }

    files.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    for (_, path) in files.into_iter().skip(KEEP_APPLIED) {
        if path != keep {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_round_trip() {
        for (b, _) in Backend::survey() {
            assert_eq!(Backend::from_name(b.name()), Some(b));
        }
        assert_eq!(Backend::from_name("KDE"), Some(Backend::Plasma));
        assert_eq!(Backend::from_name("  Plasma  "), Some(Backend::Plasma));
        assert_eq!(Backend::from_name("nonsense"), None);
    }

    #[test]
    fn which_finds_a_binary_that_definitely_exists() {
        assert!(which("sh"), "sh should be on PATH");
        assert!(!which("gitwall-definitely-not-a-real-binary"));
    }

    #[test]
    fn prune_keeps_the_active_wallpaper_even_when_it_is_oldest() {
        let dir = std::env::temp_dir().join(format!("gitwall-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Oldest file is the one we "just applied".
        let active = dir.join("active.png");
        std::fs::write(&active, b"a").unwrap();

        let mut others = Vec::new();
        for i in 0..KEEP_APPLIED + 3 {
            std::thread::sleep(Duration::from_millis(12));
            let p = dir.join(format!("other{i}.png"));
            std::fs::write(&p, b"b").unwrap();
            others.push(p);
        }

        prune_applied(&dir, &active);

        assert!(active.exists(), "the active wallpaper must never be pruned");
        let remaining = std::fs::read_dir(&dir).unwrap().count();
        assert!(
            remaining <= KEEP_APPLIED + 1,
            "expected pruning down to ~{KEEP_APPLIED}, got {remaining}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
