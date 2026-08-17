//! On-disk layout.
//!
//! Everything is keyed by git blob sha, which is content-addressed. That gives
//! us correct invalidation for free (different bytes => different key) and
//! dedupes identical wallpapers shared between repos.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::source::{Listing, RepoRef};
use crate::thumb::ThumbMeta;
use crate::{Error, Result};

/// Branches move, so a cached listing has to go stale eventually.
const LISTING_TTL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone)]
pub struct Cache {
    /// Disposable. Safe to `rm -rf` at any time.
    cache_root: PathBuf,
    /// Not disposable — holds the image the desktop is currently pointing at.
    data_root: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CachedListing {
    fetched_at: u64,
    listing: Listing,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CacheStats {
    pub thumb_count: u64,
    pub thumb_bytes: u64,
    pub full_count: u64,
    pub full_bytes: u64,
}

impl Cache {
    pub fn new() -> Result<Self> {
        let cache_root = dirs::cache_dir()
            .ok_or_else(|| Error::NoBackend("no XDG cache dir available".into()))?
            .join("gitwall");
        let data_root = dirs::data_dir()
            .ok_or_else(|| Error::NoBackend("no XDG data dir available".into()))?
            .join("gitwall");
        Self::at(cache_root, data_root)
    }

    /// Explicit roots, for tests.
    pub fn at(cache_root: PathBuf, data_root: PathBuf) -> Result<Self> {
        for d in [
            cache_root.join("thumbs"),
            cache_root.join("full"),
            cache_root.join("listings"),
            data_root.join("current"),
        ] {
            std::fs::create_dir_all(&d).map_err(|e| Error::io(&d, e))?;
        }
        Ok(Self {
            cache_root,
            data_root,
        })
    }

    /// Shard by the first two sha characters — a single directory with tens of
    /// thousands of entries gets slow to stat on some filesystems.
    fn sharded(root: &Path, sha: &str, ext: &str) -> PathBuf {
        let shard = sha.get(0..2).unwrap_or("00");
        root.join(shard).join(format!("{sha}.{ext}"))
    }

    pub fn thumb_path(&self, sha: &str) -> PathBuf {
        Self::sharded(&self.cache_root.join("thumbs"), sha, "jpg")
    }

    /// Sidecar holding source dimensions and the extracted accent colour, so a
    /// cache hit doesn't have to re-decode the image to recover them.
    pub fn thumb_meta_path(&self, sha: &str) -> PathBuf {
        Self::sharded(&self.cache_root.join("thumbs"), sha, "json")
    }

    pub fn load_thumb_meta(&self, sha: &str) -> Option<ThumbMeta> {
        let raw = std::fs::read(self.thumb_meta_path(sha)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    pub fn save_thumb_meta(&self, sha: &str, meta: &ThumbMeta) -> Result<()> {
        write_atomic(&self.thumb_meta_path(sha), &serde_json::to_vec(meta)?)
    }

    pub fn full_path(&self, sha: &str, ext: &str) -> PathBuf {
        Self::sharded(&self.cache_root.join("full"), sha, ext)
    }

    /// Where an applied wallpaper is copied to. This lives under the data dir,
    /// not the cache dir, so clearing the cache can't yank the file out from
    /// under the desktop and leave a black screen.
    pub fn wallpaper_dir(&self) -> PathBuf {
        self.data_root.join("current")
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    /// Durable state (favourites, history, the applied wallpaper) lives here —
    /// never under the cache root.
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    fn listing_path(&self, repo: &RepoRef) -> PathBuf {
        self.cache_root
            .join("listings")
            .join(format!("{}.json", repo.slug()))
    }

    pub fn load_listing(&self, repo: &RepoRef) -> Option<Listing> {
        let path = self.listing_path(repo);
        let raw = std::fs::read(&path).ok()?;
        let cached: CachedListing = serde_json::from_slice(&raw).ok()?;

        let age = now_secs().saturating_sub(cached.fetched_at);
        if age > LISTING_TTL.as_secs() {
            return None;
        }
        Some(cached.listing)
    }

    pub fn save_listing(&self, listing: &Listing) -> Result<()> {
        let path = self.listing_path(&listing.repo);
        let payload = CachedListing {
            fetched_at: now_secs(),
            listing: listing.clone(),
        };
        let bytes = serde_json::to_vec(&payload)?;
        write_atomic(&path, &bytes)
    }

    pub fn stats(&self) -> CacheStats {
        let mut s = CacheStats::default();
        let (tc, tb) = dir_stats(&self.cache_root.join("thumbs"));
        let (fc, fb) = dir_stats(&self.cache_root.join("full"));
        s.thumb_count = tc;
        s.thumb_bytes = tb;
        s.full_count = fc;
        s.full_bytes = fb;
        s
    }

    /// Drops cached bytes but deliberately leaves `current/` alone, so the
    /// active wallpaper survives.
    pub fn clear(&self) -> Result<()> {
        for sub in ["thumbs", "full", "listings"] {
            let d = self.cache_root.join(sub);
            if d.exists() {
                std::fs::remove_dir_all(&d).map_err(|e| Error::io(&d, e))?;
            }
            std::fs::create_dir_all(&d).map_err(|e| Error::io(&d, e))?;
        }
        Ok(())
    }
}

fn dir_stats(root: &Path) -> (u64, u64) {
    let mut count = 0;
    let mut bytes = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if let Ok(md) = entry.metadata() {
                count += 1;
                bytes += md.len();
            }
        }
    }
    (count, bytes)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write via a temp file + rename so a crash mid-write can never leave a
/// truncated file that later reads would treat as a valid cache hit.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let tmp = path.with_extension(format!(
        "tmp{}",
        std::process::id() as u64 ^ now_secs().rotate_left(17)
    ));
    std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(path, e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::RepoRef;

    fn tmp_cache() -> (Cache, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "gitwall-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let c = Cache::at(base.join("cache"), base.join("data")).unwrap();
        (c, base)
    }

    #[test]
    fn thumbs_are_sharded_by_sha_prefix() {
        let (c, base) = tmp_cache();
        let p = c.thumb_path("abcdef1234");
        assert!(p.ends_with("ab/abcdef1234.jpg"), "got {p:?}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn short_sha_does_not_panic_on_shard() {
        let (c, base) = tmp_cache();
        let p = c.thumb_path("a");
        assert!(p.to_string_lossy().contains("00/a.jpg"), "got {p:?}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn listing_round_trips_and_clear_keeps_the_active_wallpaper() {
        let (c, base) = tmp_cache();
        let repo = RepoRef::parse("o/r").unwrap();
        let listing = Listing {
            repo: repo.clone(),
            commit: "sha".into(),
            images: vec![],
            truncated: false,
        };
        c.save_listing(&listing).unwrap();
        assert!(c.load_listing(&repo).is_some());

        // pretend a wallpaper is in use
        let live = c.wallpaper_dir().join("live.png");
        std::fs::write(&live, b"x").unwrap();

        c.clear().unwrap();
        assert!(c.load_listing(&repo).is_none(), "listing should be gone");
        assert!(live.exists(), "clearing cache must not delete the active wallpaper");

        let _ = std::fs::remove_dir_all(base);
    }
}
