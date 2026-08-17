//! Core logic for gitwall: resolve a GitHub repo to a list of images, fetch and
//! thumbnail them lazily, and apply one as the desktop wallpaper.
//!
//! Deliberately free of any UI dependency. See `crates/gitwall-core/Cargo.toml`.

pub mod cache;
pub mod colour;
pub mod fetch;
pub mod imgur;
pub mod library;
pub mod source;
pub mod thumb;
pub mod wallhaven;
pub mod wallpaper;

pub use cache::Cache;
pub use colour::Swatch;
pub use fetch::Fetcher;
pub use library::{Favourite, Library, Visit};
pub use source::{ImageEntry, Listing, RepoRef};
pub use thumb::ThumbMeta;
pub use wallpaper::Backend;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a usable GitHub repo reference: {0}")]
    BadRepo(String),

    #[error("no images found in {0}")]
    NoImages(String),

    #[error("GitHub API returned {status} for {url}{}", detail.as_deref().map(|d| format!(" — {d}")).unwrap_or_default())]
    Api {
        status: u16,
        url: String,
        detail: Option<String>,
    },

    /// GitHub caps recursive tree listings. We surface this rather than
    /// silently showing a partial gallery.
    #[error("repo tree is too large for one listing; only part of {0} was returned")]
    TreeTruncated(String),

    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not decode image {path}: {source}")]
    Decode {
        path: String,
        #[source]
        source: image::ImageError,
    },

    #[error("bad json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("no supported wallpaper backend found. {0}")]
    NoBackend(String),

    #[error("wallpaper backend `{backend}` failed: {detail}")]
    BackendFailed { backend: String, detail: String },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}
