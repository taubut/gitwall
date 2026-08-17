//! Wallhaven search as a wallpaper source.
//!
//! This is the "just find me some wallpapers" path: type `world of warcraft`
//! rather than pasting a URL. Wallhaven's search API is public and needs no
//! key, returns native thumbnails, and reports resolution, byte size and file
//! type up front — everything the picker wants.
//!
//! Without an API key the API only ever returns SFW results, which is the
//! default we want.

use serde::Deserialize;

use crate::{Error, Result};

const API: &str = "https://wallhaven.cc/api/v1/search";

/// Results come 24 at a time. Fetching a handful of pages up front gives
/// enough to browse without turning one keystroke into fifty requests.
const PAGES: u32 = 5;

#[derive(Debug, Clone)]
pub struct Hit {
    pub id: String,
    pub ext: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
    /// Native thumbnail — a few tens of KB.
    pub thumb: String,
    pub full: String,
}

#[derive(Debug, Clone)]
pub struct Search {
    pub query: String,
    /// How many results exist in total, not how many were fetched.
    pub total: u64,
    pub hits: Vec<Hit>,
}

/* ------------------------------------------------------------- wire types */

#[derive(Deserialize)]
struct Response {
    data: Vec<Item>,
    #[serde(default)]
    meta: Meta,
}

#[derive(Deserialize, Default)]
struct Meta {
    #[serde(default)]
    total: u64,
    #[serde(default)]
    last_page: u32,
}

#[derive(Deserialize)]
struct Item {
    id: String,
    path: String,
    thumbs: Thumbs,
    #[serde(default)]
    resolution: String,
    #[serde(default)]
    file_size: u64,
    #[serde(default)]
    file_type: String,
}

#[derive(Deserialize)]
struct Thumbs {
    original: String,
}

/* ----------------------------------------------------------------- logic */

/// Split `3840x2160` into its parts. Returns zeroes if the field is missing or
/// malformed — dimensions are informational, never load-bearing.
fn parse_resolution(s: &str) -> (u32, u32) {
    match s.split_once(['x', 'X']) {
        Some((w, h)) => (
            w.trim().parse().unwrap_or(0),
            h.trim().parse().unwrap_or(0),
        ),
        None => (0, 0),
    }
}

fn ext_of(mime: &str, path: &str) -> String {
    match mime {
        "image/jpeg" => "jpg".into(),
        "image/png" => "png".into(),
        _ => path
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_else(|| "jpg".into()),
    }
}

impl From<Item> for Hit {
    fn from(i: Item) -> Self {
        let (width, height) = parse_resolution(&i.resolution);
        Hit {
            ext: ext_of(&i.file_type, &i.path),
            width,
            height,
            size: i.file_size,
            thumb: i.thumbs.original,
            full: i.path,
            id: i.id,
        }
    }
}

/// Does this input look like a search rather than a URL?
///
/// Deliberately the *last* thing tried: anything that parses as a GitHub or
/// Imgur reference goes to that source instead. A bare phrase with no host and
/// no `owner/repo` shape is a query.
pub fn looks_like_query(input: &str) -> bool {
    let t = input.trim();
    !t.is_empty()
        && !t.contains("://")
        && !t.contains("github.com")
        && !t.contains("imgur.com")
        && crate::RepoRef::parse(t).is_err()
}

pub struct WallhavenClient {
    http: reqwest::Client,
    /// Optional; only needed to reach anything beyond SFW, which this does not
    /// ask for.
    api_key: Option<String>,
}

impl WallhavenClient {
    pub fn new(http: reqwest::Client) -> Self {
        let api_key = std::env::var("WALLHAVEN_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty());
        Self { http, api_key }
    }

    async fn page(&self, query: &str, page: u32) -> Result<Response> {
        let mut req = self.http.get(API).query(&[
            ("q", query),
            ("sorting", "favorites"),
            ("order", "desc"),
            ("page", &page.to_string()),
        ]);
        if let Some(k) = &self.api_key {
            req = req.query(&[("apikey", k)]);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                url: format!("{API}?q={query}&page={page}"),
                // 429 is the one a user will actually hit: 45 requests/minute.
                detail: (status.as_u16() == 429)
                    .then(|| "Wallhaven rate limit (45 requests/minute) — wait a moment".into()),
            });
        }

        resp.json::<Response>().await.map_err(Error::Http)
    }

    pub async fn search(&self, query: &str) -> Result<Search> {
        let first = self.page(query, 1).await?;
        let total = first.meta.total;
        let last = first.meta.last_page.max(1);

        let mut hits: Vec<Hit> = first.data.into_iter().map(Hit::from).collect();
        if hits.is_empty() {
            return Err(Error::NoImages(format!("wallhaven search for {query:?}")));
        }

        // Pull a few more pages so there is something to scroll through.
        let extra = (2..=PAGES.min(last)).map(|p| self.page(query, p));
        for page in futures::future::join_all(extra).await.into_iter().flatten() {
            hits.extend(page.data.into_iter().map(Hit::from));
        }

        Ok(Search {
            query: query.to_string(),
            total,
            hits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_phrase_is_a_query_but_a_repo_or_url_is_not() {
        assert!(looks_like_query("world of warcraft"));
        assert!(looks_like_query("cyberpunk"));

        // Anything another source can claim must win.
        assert!(!looks_like_query("owner/repo"));
        assert!(!looks_like_query("https://github.com/o/r"));
        assert!(!looks_like_query("github.com/o/r/tree/main/images"));
        assert!(!looks_like_query("https://imgur.com/gallery/x-1Ur1STy"));
        assert!(!looks_like_query("   "));
    }

    #[test]
    fn resolution_parses_and_degrades_quietly() {
        assert_eq!(parse_resolution("3840x2160"), (3840, 2160));
        assert_eq!(parse_resolution("10000x4235"), (10000, 4235));
        assert_eq!(parse_resolution(""), (0, 0));
        assert_eq!(parse_resolution("wide"), (0, 0));
    }

    #[test]
    fn extension_comes_from_the_mime_type_with_a_path_fallback() {
        assert_eq!(ext_of("image/jpeg", "x.jpg"), "jpg");
        assert_eq!(ext_of("image/png", "x.png"), "png");
        assert_eq!(ext_of("", "https://w.wallhaven.cc/full/e7/a-b.webp"), "webp");
    }

    #[test]
    fn parses_a_real_response_shape() {
        // Trimmed from an actual /api/v1/search response.
        let body = r#"{
          "data": [{
            "id": "e77g8k",
            "url": "https://wallhaven.cc/w/e77g8k",
            "path": "https://w.wallhaven.cc/full/e7/wallhaven-e77g8k.jpg",
            "thumbs": { "original": "https://th.wallhaven.cc/orig/e7/e77g8k.jpg" },
            "resolution": "3840x2160",
            "file_size": 1600000,
            "file_type": "image/jpeg",
            "purity": "sfw"
          }],
          "meta": { "total": 1243, "last_page": 52, "per_page": 24 }
        }"#;

        let r: Response = serde_json::from_str(body).expect("should parse");
        assert_eq!(r.meta.total, 1243);
        assert_eq!(r.meta.last_page, 52);

        let hit = Hit::from(r.data.into_iter().next().unwrap());
        assert_eq!(hit.id, "e77g8k");
        assert_eq!((hit.width, hit.height), (3840, 2160));
        assert_eq!(hit.ext, "jpg");
        assert!(hit.thumb.contains("th.wallhaven.cc"), "uses the native thumbnail");
        assert!(hit.full.contains("w.wallhaven.cc/full"));
    }

    #[test]
    fn a_response_missing_meta_still_parses() {
        let body = r#"{"data":[]}"#;
        let r: Response = serde_json::from_str(body).expect("meta must be optional");
        assert_eq!(r.meta.total, 0);
    }
}
