//! Imgur galleries and albums as a wallpaper source.
//!
//! Imgur's official API returns 429 without a registered Client-ID, so this
//! reads the gallery page instead, which embeds the whole post as JSON in a
//! `window.postDataJSON="..."` assignment. That is a scrape and Imgur can
//! change it, so every failure mode here reports what actually went wrong
//! rather than quietly handing back an empty gallery.
//!
//! Two things make Imgur a better source than a git repo, once parsed:
//! dimensions and byte sizes are known up front, and Imgur serves native
//! thumbnails — a 3840x2160 image whose original is 1.1 MB has a 48 KB
//! preview, so browsing a 545 MB album costs about 24 MB.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Imgur rejects unfamiliar clients on the web endpoints, so the page fetch
/// has to look like a browser. Only used for the HTML page, not for images.
const BROWSER_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

const MARKER: &str = "window.postDataJSON=\"";

/// What an Imgur URL points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImgurRef {
    /// A single gallery post or album, identified by its hash.
    Album(String),
    /// A tag feed — many separate posts, not one collection.
    Tag(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImgurItem {
    pub id: String,
    pub ext: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub size: u64,
}

impl ImgurItem {
    /// Full-resolution original.
    pub fn full_url(&self) -> String {
        format!("https://i.imgur.com/{}.{}", self.id, self.ext)
    }

    /// Native ~640px preview. Imgur always serves these as jpeg regardless of
    /// the original format, which is exactly what we want for browsing.
    pub fn preview_url(&self) -> String {
        format!("https://i.imgur.com/{}l.jpg", self.id)
    }
}

#[derive(Debug, Clone)]
pub struct ImgurAlbum {
    pub hash: String,
    pub title: String,
    pub items: Vec<ImgurItem>,
}

impl ImgurAlbum {
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }
}

/// Recognise the Imgur URL shapes people paste.
///
/// Gallery slugs carry the hash as the final `-` segment
/// (`4k-wallpaper-dump-1Ur1STy`), which is why this can't just take the last
/// path component wholesale.
pub fn parse_ref(input: &str) -> Option<ImgurRef> {
    let mut s = input.trim();
    for p in ["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest;
            break;
        }
    }
    s = s.strip_prefix("www.").unwrap_or(s);
    s = s.strip_prefix("m.").unwrap_or(s);
    let s = s.strip_prefix("imgur.com/")?;
    let s = s.split(['?', '#']).next().unwrap_or(s).trim_matches('/');

    let mut parts = s.split('/').filter(|p| !p.is_empty());
    let first = parts.next()?;

    match first {
        "t" => {
            // /t/<tag> or /t/<tag>/<posthash>
            let tag = parts.next()?;
            match parts.next() {
                Some(post) => Some(ImgurRef::Album(hash_of(post)?)),
                None => Some(ImgurRef::Tag(tag.to_string())),
            }
        }
        "gallery" | "a" => parts.next().and_then(hash_of).map(ImgurRef::Album),
        other => hash_of(other).map(ImgurRef::Album),
    }
}

/// Pull the hash out of a slug like `4k-wallpaper-dump-1Ur1STy`.
fn hash_of(segment: &str) -> Option<String> {
    let candidate = segment.rsplit('-').next().unwrap_or(segment);
    let ok = !candidate.is_empty()
        && candidate.len() >= 5
        && candidate.len() <= 12
        && candidate.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then(|| candidate.to_string())
}

/// Decode a JavaScript double-quoted string literal.
///
/// `serde_json` can't do this: JS allows escapes such as `\'` that JSON
/// rejects, and the payload is a JSON document escaped *inside* a JS string.
fn js_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(ch) => out.push(ch),
                    None => out.push_str(&hex),
                }
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

/// Extract and parse the embedded post JSON from a gallery page.
pub fn extract_post_json(html: &str) -> Result<serde_json::Value> {
    let start = html.find(MARKER).ok_or_else(|| Error::Api {
        status: 200,
        url: "imgur gallery page".into(),
        detail: Some(
            "no postDataJSON in the page — Imgur may have changed their markup, \
             or the album is private"
                .into(),
        ),
    })? + MARKER.len();

    // Walk to the closing quote, honouring backslash escapes.
    let bytes = html.as_bytes();
    let mut i = start;
    let mut end = None;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => {
                end = Some(i);
                break;
            }
            _ => i += 1,
        }
    }
    let end = end.ok_or_else(|| Error::Api {
        status: 200,
        url: "imgur gallery page".into(),
        detail: Some("postDataJSON was not terminated".into()),
    })?;

    let decoded = js_unescape(&html[start..end]);
    serde_json::from_str(&decoded).map_err(Error::Json)
}

/// Turn the post JSON into image items, skipping anything that isn't a still
/// image (Imgur posts can mix in video and gifs).
pub fn items_from_json(v: &serde_json::Value) -> Vec<ImgurItem> {
    let media = match v.get("media").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return Vec::new(),
    };

    media
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let ext = m.get("ext")?.as_str()?.to_ascii_lowercase();

            let mime = m.get("mime_type").and_then(|x| x.as_str()).unwrap_or("");
            if mime.starts_with("video") || matches!(ext.as_str(), "mp4" | "webm" | "gifv") {
                return None;
            }

            let name = m
                .get("name")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&id)
                .to_string();

            Some(ImgurItem {
                width: m.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                height: m.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                size: m.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
                id,
                ext,
                name,
            })
        })
        .collect()
}

pub struct ImgurClient {
    http: reqwest::Client,
}

impl ImgurClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub async fn resolve(&self, reference: &ImgurRef) -> Result<ImgurAlbum> {
        let hash = match reference {
            ImgurRef::Album(h) => h.clone(),
            ImgurRef::Tag(tag) => {
                return Err(Error::NoImages(format!(
                    "imgur.com/t/{tag} is a tag feed of many separate posts, not one album — \
                     open a specific post from it instead"
                )))
            }
        };

        let url = format!("https://imgur.com/gallery/{hash}");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, BROWSER_UA)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                url: url.clone(),
                detail: (status.as_u16() == 404)
                    .then(|| "no such album, or it is private".to_string()),
            });
        }

        let html = resp.text().await?;
        let json = extract_post_json(&html)?;

        let title = json
            .get("title")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&hash)
            .to_string();

        let items = items_from_json(&json);
        if items.is_empty() {
            return Err(Error::NoImages(format!("imgur album {hash}")));
        }

        Ok(ImgurAlbum { hash, title, items })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_gallery_url_shapes() {
        assert_eq!(
            parse_ref("https://imgur.com/gallery/4k-wallpaper-dump-1Ur1STy"),
            Some(ImgurRef::Album("1Ur1STy".into())),
            "the hash is the last dash-segment of a slug"
        );
        assert_eq!(
            parse_ref("https://imgur.com/a/1Ur1STy"),
            Some(ImgurRef::Album("1Ur1STy".into()))
        );
        assert_eq!(
            parse_ref("imgur.com/gallery/1Ur1STy?foo=1#x"),
            Some(ImgurRef::Album("1Ur1STy".into())),
            "query and fragment must be stripped"
        );
        assert_eq!(
            parse_ref("https://www.imgur.com/1Ur1STy"),
            Some(ImgurRef::Album("1Ur1STy".into()))
        );
    }

    #[test]
    fn tag_feeds_are_recognised_but_kept_distinct_from_albums() {
        assert_eq!(
            parse_ref("https://imgur.com/t/wallpaper_dump"),
            Some(ImgurRef::Tag("wallpaper_dump".into()))
        );
        // A post *within* a tag is still a normal album.
        assert_eq!(
            parse_ref("https://imgur.com/t/wallpaper_dump/abcdefg"),
            Some(ImgurRef::Album("abcdefg".into()))
        );
    }

    #[test]
    fn ignores_urls_that_are_not_imgur() {
        assert_eq!(parse_ref("https://github.com/o/r"), None);
        assert_eq!(parse_ref("o/r"), None);
        assert_eq!(parse_ref(""), None);
    }

    #[test]
    fn js_unescape_handles_what_json_cannot() {
        // `\/` is legal in a JS string but not something serde_json will accept
        // as part of the outer literal.
        assert_eq!(js_unescape(r#"a\/b"#), "a/b");
        assert_eq!(js_unescape(r#"say \"hi\""#), r#"say "hi""#);
        assert_eq!(js_unescape(r#"back\\slash"#), r"back\slash");
        assert_eq!(js_unescape(r#"Aé"#), "Aé");
    }

    fn fixture() -> String {
        // Mirrors the real page: a JSON document escaped inside a JS string.
        let json = r#"{"id":"1Ur1STy","title":"4K wallpaper dump!","image_count":3,"media":[
            {"id":"EilecxS","ext":"jpeg","width":3840,"height":2160,"size":477732,"mime_type":"image/jpeg"},
            {"id":"PE4G2Mw","ext":"png","width":3840,"height":2160,"size":3661041,"mime_type":"image/png"},
            {"id":"MOVIEid","ext":"mp4","width":1920,"height":1080,"size":900,"mime_type":"video/mp4"}
        ]}"#;
        let escaped = json.replace('\\', r"\\").replace('"', r#"\""#);
        format!("<html><script>window.postDataJSON=\"{escaped}\";</script></html>")
    }

    #[test]
    fn parses_an_album_page_and_drops_video() {
        let v = extract_post_json(&fixture()).expect("should extract");
        assert_eq!(v.get("title").unwrap().as_str().unwrap(), "4K wallpaper dump!");

        let items = items_from_json(&v);
        assert_eq!(items.len(), 2, "the mp4 entry must be skipped");
        assert_eq!(items[0].id, "EilecxS");
        assert_eq!(items[0].width, 3840);
        assert_eq!(items[0].size, 477732);
    }

    #[test]
    fn builds_native_preview_and_full_urls() {
        let it = ImgurItem {
            id: "EilecxS".into(),
            ext: "jpeg".into(),
            name: "EilecxS".into(),
            width: 3840,
            height: 2160,
            size: 477732,
        };
        assert_eq!(it.full_url(), "https://i.imgur.com/EilecxS.jpeg");
        // The preview is always jpg, whatever the original format.
        assert_eq!(it.preview_url(), "https://i.imgur.com/EilecxSl.jpg");
    }

    #[test]
    fn a_page_without_the_marker_reports_why() {
        let err = extract_post_json("<html>nothing here</html>").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("postDataJSON"),
            "error should name the missing marker, got: {msg}"
        );
    }
}
