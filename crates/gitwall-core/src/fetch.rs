//! Bounded, deduplicated, retrying downloader.
//!
//! Three properties matter here, all of them driven by the fact that a fast
//! scroll can ask for the same hundred images several times over:
//!
//! 1. Bounded concurrency, so scrolling doesn't open 400 sockets.
//! 2. In-flight dedup, so two panels asking for one image download it once.
//! 3. Cache-first, so the second pass through a repo touches the network zero
//!    times.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};

use crate::cache::{write_atomic, Cache};
use crate::source::{ImageEntry, Listing};
use crate::thumb::{self, ThumbMeta, THUMB_MAX};
use crate::{Error, Result};

/// Enough to saturate a home connection without hammering the CDN.
const MAX_CONCURRENT: usize = 6;
/// Nothing in a wallpaper repo should be near this. Guards against a repo with
/// a stray multi-gigabyte file eating the disk.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const ATTEMPTS: usize = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Pick the file extension for a cached original.
///
/// This has to be right: the applied wallpaper is named with it, and the
/// desktop classifies files by extension. A JPEG saved as `.img` is reported by
/// xdg-mime as `application/vnd.efi.img` — a disk image — and GNOME silently
/// refuses to use it as a background even though the pixels load fine.
///
/// The URL is the most reliable source, since it names the actual file being
/// fetched. Labels are not: Imgur and Wallhaven identify images by a bare id
/// with no extension at all.
fn ext_for(label: &str, urls: &[String]) -> String {
    let from = |s: &str| -> Option<String> {
        // Drop any query or fragment before looking for the extension.
        let path = s.split(['?', '#']).next().unwrap_or(s);
        let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
        crate::source::IMAGE_EXTS
            .contains(&ext.as_str())
            .then_some(ext)
    };

    urls.iter()
        .find_map(|u| from(u))
        .or_else(|| from(label))
        // Never "img". A plausible image extension is far safer than one the
        // desktop reads as a disk image.
        .unwrap_or_else(|| "jpg".to_string())
}

pub struct Fetcher {
    http: reqwest::Client,
    sem: Arc<Semaphore>,
    /// One lock per cache key, so concurrent requests for the same image
    /// collapse into a single download.
    inflight: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Fetcher {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            sem: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    async fn key_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut map = self.inflight.lock().await;
        map.entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn release_key(&self, key: &str) {
        let mut map = self.inflight.lock().await;
        // Only drop the entry if we hold the last reference, otherwise another
        // waiter is about to use it.
        if let Some(l) = map.get(key) {
            if Arc::strong_count(l) <= 1 {
                map.remove(key);
            }
        }
    }

    /// Download with retries, falling back from the CDN to raw.githubusercontent.
    async fn download(&self, urls: &[String], label: &str) -> Result<Vec<u8>> {
        let _permit = self.sem.acquire().await.expect("semaphore never closed");

        let mut last: Option<Error> = None;

        for url in urls {
            for attempt in 0..ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
                }

                let resp = self.http.get(url).timeout(REQUEST_TIMEOUT).send().await;

                let resp = match resp {
                    Ok(r) => r,
                    Err(e) => {
                        last = Some(Error::Http(e));
                        continue;
                    }
                };

                let status = resp.status();
                if status.is_success() {
                    if let Some(len) = resp.content_length() {
                        if len > MAX_BYTES {
                            return Err(Error::Api {
                                status: status.as_u16(),
                                url: url.clone(),
                                detail: Some(format!(
                                    "{label} is {len} bytes, over the {MAX_BYTES} byte limit"
                                )),
                            });
                        }
                    }
                    match resp.bytes().await {
                        Ok(b) if !b.is_empty() => return Ok(b.to_vec()),
                        Ok(_) => {
                            last = Some(Error::Api {
                                status: 200,
                                url: url.clone(),
                                detail: Some("empty response body".into()),
                            });
                        }
                        Err(e) => last = Some(Error::Http(e)),
                    }
                    continue;
                }

                last = Some(Error::Api {
                    status: status.as_u16(),
                    url: url.clone(),
                    detail: None,
                });

                // 4xx other than 429 won't fix itself; move to the next URL.
                if status.is_client_error() && status.as_u16() != 429 {
                    break;
                }
            }
        }

        Err(last.unwrap_or_else(|| Error::Api {
            status: 0,
            url: urls.first().cloned().unwrap_or_default(),
            detail: Some("no URLs to try".into()),
        }))
    }

    /// Thumbnail path plus its metadata, downloading and generating if needed.
    ///
    /// A thumbnail without its sidecar counts as a miss: the accent colour and
    /// source dimensions live only in the sidecar, and they can't be recovered
    /// from the downscaled JPEG.
    pub async fn thumb(
        &self,
        cache: &Cache,
        listing: &Listing,
        entry: &ImageEntry,
    ) -> Result<(PathBuf, ThumbMeta)> {
        self.thumb_at(cache, &entry.sha, &entry.path, listing.urls(entry))
            .await
    }

    /// As `thumb`, but takes the URLs directly instead of deriving them from a
    /// listing — favourites span repos, so there is no single listing to ask.
    pub async fn thumb_at(
        &self,
        cache: &Cache,
        sha: &str,
        label: &str,
        urls: [String; 2],
    ) -> Result<(PathBuf, ThumbMeta)> {
        let entry_sha = sha;
        let path = cache.thumb_path(entry_sha);
        if path.exists() {
            if let Some(meta) = cache.load_thumb_meta(entry_sha) {
                return Ok((path, meta));
            }
        }

        let key = format!("thumb:{entry_sha}");
        let lock = self.key_lock(&key).await;
        let _guard = lock.lock().await;

        // Another task may have finished it while we waited for the lock.
        if path.exists() {
            if let Some(meta) = cache.load_thumb_meta(entry_sha) {
                self.release_key(&key).await;
                return Ok((path, meta));
            }
        }

        let result = async {
            let bytes = self.download(&urls, label).await?;

            let out = path.clone();
            let owned_label = label.to_string();
            // Decode, resize and accent extraction are CPU-bound; keep them off
            // the async runtime.
            let meta = tokio::task::spawn_blocking(move || {
                thumb::write_thumbnail(&bytes, &out, THUMB_MAX, &owned_label)
            })
            .await
            .map_err(|e| Error::Api {
                status: 0,
                url: label.to_string(),
                detail: Some(format!("thumbnail task panicked: {e}")),
            })??;

            cache.save_thumb_meta(entry_sha, &meta)?;
            Ok((path.clone(), meta))
        }
        .await;

        self.release_key(&key).await;
        result
    }

    /// Path to the full-resolution image, downloading if needed. Only called
    /// for an image the user actually selects.
    pub async fn full(
        &self,
        cache: &Cache,
        listing: &Listing,
        entry: &ImageEntry,
    ) -> Result<PathBuf> {
        self.full_at(cache, &entry.sha, &entry.path, listing.urls(entry))
            .await
    }

    /// As `full`, but takes the URLs directly. See `thumb_at`.
    pub async fn full_at(
        &self,
        cache: &Cache,
        sha: &str,
        label: &str,
        urls: [String; 2],
    ) -> Result<PathBuf> {
        let ext = ext_for(label, &urls);
        let path = cache.full_path(sha, &ext);
        if path.exists() {
            return Ok(path);
        }

        let key = format!("full:{sha}");
        let lock = self.key_lock(&key).await;
        let _guard = lock.lock().await;

        if path.exists() {
            self.release_key(&key).await;
            return Ok(path);
        }

        let result = async {
            let bytes = self.download(&urls, label).await?;
            write_atomic(&path, &bytes)?;
            Ok(path.clone())
        }
        .await;

        self.release_key(&key).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_comes_from_the_url_not_the_label() {
        // Imgur and Wallhaven label images with a bare id.
        assert_eq!(
            ext_for("EilecxS", &["https://i.imgur.com/EilecxS.jpeg".into()]),
            "jpeg"
        );
        assert_eq!(
            ext_for(
                "e77g8k",
                &["https://w.wallhaven.cc/full/e7/wallhaven-e77g8k.jpg".into()]
            ),
            "jpg"
        );
        // GitHub labels are paths and work either way.
        assert_eq!(
            ext_for(
                "images/foo.png",
                &["https://cdn.jsdelivr.net/gh/o/r@abc/images/foo.png".into()]
            ),
            "png"
        );
    }

    #[test]
    fn never_yields_img_which_the_desktop_reads_as_a_disk_image() {
        // No extension anywhere: must still be something usable.
        let e = ext_for("EilecxS", &["https://i.imgur.com/EilecxS".into()]);
        assert_ne!(e, "img", "`.img` is detected as application/vnd.efi.img");
        assert!(crate::source::IMAGE_EXTS.contains(&e.as_str()));
    }

    #[test]
    fn query_strings_and_junk_extensions_are_ignored() {
        assert_eq!(
            ext_for("x", &["https://host/a/b.png?w=400&fit=cover".into()]),
            "png"
        );
        // ".com" is not an image extension, so it must not be taken as one.
        assert_eq!(ext_for("x", &["https://i.imgur.com".into()]), "jpg");
        // Falls through to the label when the URL has nothing.
        assert_eq!(ext_for("photo.webp", &["https://host/opaque".into()]), "webp");
    }

    #[tokio::test]
    async fn key_locks_are_shared_then_reclaimed() {
        let f = Fetcher::new(reqwest::Client::new());

        let a = f.key_lock("thumb:abc").await;
        let b = f.key_lock("thumb:abc").await;
        assert!(Arc::ptr_eq(&a, &b), "same key must hand back the same lock");

        let c = f.key_lock("thumb:xyz").await;
        assert!(!Arc::ptr_eq(&a, &c), "different keys must not share a lock");

        drop(b);
        drop(c);
        f.release_key("thumb:abc").await;
        assert_eq!(
            f.inflight.lock().await.len(),
            2,
            "entry still referenced by `a`, so it must survive"
        );

        drop(a);
        f.release_key("thumb:abc").await;
        f.release_key("thumb:xyz").await;
        assert!(
            f.inflight.lock().await.is_empty(),
            "map must not grow without bound"
        );
    }
}
