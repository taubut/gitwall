//! Turning whatever the user typed into a concrete, pinned list of images.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Extensions we treat as wallpapers. `avif` is listed because repos use it,
/// but note the `image` crate feature set in Cargo.toml does not decode it —
/// such entries will fail at thumbnail time and be skipped, not crash.
pub const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// Wallpaper repos keep logos, badges and separator bars alongside the actual
/// wallpapers (this repo has a 490-byte `assets/bar.png`). Nothing that small
/// is a wallpaper, and they read as broken panels in the gallery.
const MIN_WALLPAPER_BYTES: u64 = 16 * 1024;

const API: &str = "https://api.github.com";
const UA: &str = concat!("gitwall/", env!("CARGO_PKG_VERSION"));

/// A repo plus optional ref and subdirectory, as the user expressed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
    /// Branch, tag or sha. `None` means "whatever the default branch is".
    pub git_ref: Option<String>,
    /// Only list images under this repo-relative directory.
    pub subdir: Option<String>,
}

impl RepoRef {
    /// Accepts the many shapes a person might paste:
    ///
    /// - `https://github.com/owner/repo`
    /// - `https://github.com/owner/repo/tree/main/images`
    /// - `https://github.com/owner/repo/blob/main/images/a.png`
    /// - `github.com/owner/repo.git`
    /// - `git@github.com:owner/repo.git`
    /// - `owner/repo`
    ///
    /// Known limit: a branch name containing `/` (`feature/x`) is
    /// indistinguishable from `branch + subdir` in a `/tree/` URL. We take the
    /// first segment as the ref; pass the subdir separately if that bites.
    pub fn parse(input: &str) -> Result<Self> {
        let bad = || Error::BadRepo(input.to_string());

        let mut s = input.trim();
        if s.is_empty() {
            return Err(bad());
        }

        for scheme in ["https://", "http://"] {
            if let Some(rest) = s.strip_prefix(scheme) {
                s = rest;
                break;
            }
        }
        s = s.strip_prefix("www.").unwrap_or(s);
        s = match s.strip_prefix("git@github.com:") {
            Some(rest) => rest,
            None => s.strip_prefix("github.com/").unwrap_or(s),
        };
        let s = s.trim_matches('/');

        let mut parts = s.split('/').filter(|p| !p.is_empty());
        let owner = parts.next().ok_or_else(bad)?;
        let repo = parts.next().ok_or_else(bad)?.trim_end_matches(".git");

        if owner.is_empty() || repo.is_empty() {
            return Err(bad());
        }

        let mut git_ref = None;
        let mut subdir = None;

        match parts.next() {
            Some(kind @ ("tree" | "blob")) => {
                git_ref = parts.next().filter(|r| !r.is_empty()).map(str::to_string);
                let rest: Vec<&str> = parts.collect();
                if !rest.is_empty() {
                    // For /blob/ the last segment is the file itself, so the
                    // directory the user was looking at is everything before it.
                    let dir = if kind == "blob" {
                        &rest[..rest.len() - 1]
                    } else {
                        &rest[..]
                    };
                    if !dir.is_empty() {
                        subdir = Some(dir.join("/"));
                    }
                }
            }
            // Anything else after owner/repo isn't something we can interpret,
            // so ignore it rather than guessing wrong.
            _ => {}
        }

        Ok(RepoRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
            git_ref,
            subdir,
        })
    }

    /// Filesystem-safe key for cached listings.
    pub fn slug(&self) -> String {
        let mut s = format!("{}__{}", self.owner, self.repo);
        if let Some(r) = &self.git_ref {
            s.push_str("__");
            s.push_str(&sanitize(r));
        }
        if let Some(d) = &self.subdir {
            s.push_str("__");
            s.push_str(&sanitize(d));
        }
        s
    }

    pub fn display(&self) -> String {
        let mut s = format!("{}/{}", self.owner, self.repo);
        if let Some(r) = &self.git_ref {
            s.push('@');
            s.push_str(r);
        }
        if let Some(d) = &self.subdir {
            s.push('/');
            s.push_str(d);
        }
        s
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect()
}

/// One image blob in the repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEntry {
    /// Repo-relative path, e.g. `images/foo.png`.
    pub path: String,
    /// Basename without extension, for display.
    pub name: String,
    /// Git blob sha. Content-addressed, so it doubles as the cache key and
    /// dedupes identical images across different repos.
    pub sha: String,
    pub size: u64,
}

/// A repo resolved to a pinned commit and a concrete image list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Listing {
    pub repo: RepoRef,
    /// Resolved commit sha. Pinning here means CDN URLs and cached thumbnails
    /// stay correct even if the branch moves under us.
    pub commit: String,
    pub images: Vec<ImageEntry>,
    /// GitHub caps recursive tree responses. If this is set the gallery is
    /// incomplete and the UI should say so.
    pub truncated: bool,
}

impl Listing {
    pub fn urls(&self, e: &ImageEntry) -> [String; 2] {
        image_urls(&self.repo.owner, &self.repo.repo, &self.commit, &e.path)
    }

    /// jsDelivr — much faster than raw.githubusercontent and doesn't touch the
    /// API rate limit. Won't serve files over 20 MB; `raw_url` covers that.
    pub fn cdn_url(&self, e: &ImageEntry) -> String {
        self.urls(e)[0].clone()
    }

    pub fn raw_url(&self, e: &ImageEntry) -> String {
        self.urls(e)[1].clone()
    }
}

/// Build the two URLs an image can be fetched from: jsDelivr first (fast, and
/// it doesn't touch the API rate limit), raw.githubusercontent as the fallback
/// for anything jsDelivr won't serve.
pub fn image_urls(owner: &str, repo: &str, commit: &str, path: &str) -> [String; 2] {
    let p = encode_path(path);
    [
        format!("https://cdn.jsdelivr.net/gh/{owner}/{repo}@{commit}/{p}"),
        format!("https://raw.githubusercontent.com/{owner}/{repo}/{commit}/{p}"),
    ]
}

/// Percent-encode everything outside the unreserved set, but keep `/` so path
/// structure survives.
pub(crate) fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn is_image(path: &str) -> bool {
    match path.rsplit_once('.') {
        Some((_, ext)) => IMAGE_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

pub struct GithubClient {
    http: reqwest::Client,
    token: Option<String>,
}

impl GithubClient {
    pub fn new(http: reqwest::Client) -> Self {
        // A token isn't required — we only make 2 API calls per repo and the
        // image bytes come from the CDN — but it lifts 60/hr to 5000/hr for
        // anyone flipping between many repos.
        let token = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("GH_TOKEN"))
            .ok()
            .filter(|t| !t.trim().is_empty());
        Self { http, token }
    }

    pub fn with_token(http: reqwest::Client, token: Option<String>) -> Self {
        Self { http, token }
    }

    async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let mut req = self
            .http
            .get(url)
            .header(reqwest::header::USER_AGENT, UA)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");

        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() {
            let remaining = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);

            let body = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string));

            // The rate-limit 403 is the one failure a user will actually hit,
            // so make it self-explanatory instead of "403 Forbidden".
            let detail = if status.as_u16() == 403 && remaining.as_deref() == Some("0") {
                Some(
                    "GitHub API rate limit reached (60/hr unauthenticated). \
                     Set GITHUB_TOKEN to raise it to 5000/hr."
                        .to_string(),
                )
            } else {
                msg
            };

            return Err(Error::Api {
                status: status.as_u16(),
                url: url.to_string(),
                detail,
            });
        }

        Ok(resp.json().await?)
    }

    /// Resolve a repo reference into a pinned list of images.
    ///
    /// Costs two API calls regardless of repo size: one to pin the ref to a
    /// commit, one to pull the whole tree.
    pub async fn resolve(&self, repo: &RepoRef) -> Result<Listing> {
        // `HEAD` resolves to the default branch, which saves us a separate
        // lookup of what that branch is called.
        let git_ref = repo.git_ref.as_deref().unwrap_or("HEAD");
        let commit_url = format!("{API}/repos/{}/{}/commits/{}", repo.owner, repo.repo, git_ref);
        let commit_json = self.get_json(&commit_url).await?;

        let commit = commit_json
            .get("sha")
            .and_then(|s| s.as_str())
            .ok_or_else(|| Error::Api {
                status: 200,
                url: commit_url.clone(),
                detail: Some("commit response had no sha".into()),
            })?
            .to_string();

        let tree_url = format!(
            "{API}/repos/{}/{}/git/trees/{}?recursive=1",
            repo.owner, repo.repo, commit
        );
        let tree_json = self.get_json(&tree_url).await?;

        let truncated = tree_json
            .get("truncated")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);

        let prefix = repo.subdir.as_ref().map(|d| format!("{}/", d.trim_matches('/')));

        let mut images: Vec<ImageEntry> = tree_json
            .get("tree")
            .and_then(|t| t.as_array())
            .map(|arr| arr.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("blob"))
            .filter_map(|e| {
                let path = e.get("path")?.as_str()?;
                if !is_image(path) {
                    return None;
                }
                if let Some(p) = &prefix {
                    if !path.starts_with(p.as_str()) {
                        return None;
                    }
                }
                let size = e.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
                // Size 0 means GitHub didn't report one; keep those rather than
                // silently dropping something that might be a real wallpaper.
                if size > 0 && size < MIN_WALLPAPER_BYTES {
                    return None;
                }
                let name = path
                    .rsplit('/')
                    .next()
                    .unwrap_or(path)
                    .rsplit_once('.')
                    .map(|(stem, _)| stem)
                    .unwrap_or(path)
                    .to_string();

                Some(ImageEntry {
                    path: path.to_string(),
                    name,
                    sha: e.get("sha")?.as_str()?.to_string(),
                    size,
                })
            })
            .collect();

        if images.is_empty() {
            return Err(if truncated {
                Error::TreeTruncated(repo.display())
            } else {
                Error::NoImages(repo.display())
            });
        }

        images.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(Listing {
            repo: repo.clone(),
            commit,
            images,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoRef {
        RepoRef::parse(s).expect("should parse")
    }

    #[test]
    fn parses_the_url_shapes_people_actually_paste() {
        let plain = p("https://github.com/D3Ext/aesthetic-wallpapers");
        assert_eq!(plain.owner, "D3Ext");
        assert_eq!(plain.repo, "aesthetic-wallpapers");
        assert_eq!(plain.git_ref, None);
        assert_eq!(plain.subdir, None);

        // the exact URL from the original request
        let tree = p("https://github.com/D3Ext/aesthetic-wallpapers/tree/main");
        assert_eq!(tree.git_ref.as_deref(), Some("main"));
        assert_eq!(tree.subdir, None);

        let sub = p("https://github.com/D3Ext/aesthetic-wallpapers/tree/main/images");
        assert_eq!(sub.git_ref.as_deref(), Some("main"));
        assert_eq!(sub.subdir.as_deref(), Some("images"));

        let nested = p("github.com/o/r/tree/main/a/b/c");
        assert_eq!(nested.subdir.as_deref(), Some("a/b/c"));

        assert_eq!(p("owner/repo").repo, "repo");
        assert_eq!(p("git@github.com:owner/repo.git").repo, "repo");
        assert_eq!(p("https://github.com/owner/repo.git/").repo, "repo");
        assert_eq!(p("  https://www.github.com/owner/repo  ").owner, "owner");
    }

    #[test]
    fn blob_urls_drop_the_filename_and_keep_the_directory() {
        let b = p("https://github.com/o/r/blob/main/images/foo.png");
        assert_eq!(b.git_ref.as_deref(), Some("main"));
        assert_eq!(b.subdir.as_deref(), Some("images"));

        // a file at the repo root leaves no subdir behind
        let root = p("https://github.com/o/r/blob/main/foo.png");
        assert_eq!(root.subdir, None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(RepoRef::parse("").is_err());
        assert!(RepoRef::parse("   ").is_err());
        assert!(RepoRef::parse("owner").is_err());
        assert!(RepoRef::parse("https://github.com/owner").is_err());
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert!(is_image("a/b/C.PNG"));
        assert!(is_image("x.JpEg"));
        assert!(!is_image("README.md"));
        assert!(!is_image("noextension"));
        assert!(!is_image("dir.png/file"));
    }

    #[test]
    fn spaces_and_unicode_survive_url_building() {
        let listing = Listing {
            repo: p("o/r"),
            commit: "abc123".into(),
            images: vec![],
            truncated: false,
        };
        let e = ImageEntry {
            path: "images/a b&c.png".into(),
            name: "a b&c".into(),
            sha: "deadbeef".into(),
            size: 1,
        };
        assert_eq!(
            listing.cdn_url(&e),
            "https://cdn.jsdelivr.net/gh/o/r@abc123/images/a%20b%26c.png"
        );
        assert!(listing.raw_url(&e).ends_with("/images/a%20b%26c.png"));
    }

    #[test]
    fn slug_is_filesystem_safe() {
        let r = p("https://github.com/o/r/tree/feat/images");
        let slug = r.slug();
        assert!(!slug.contains('/'), "slug must not contain path separators: {slug}");
    }
}
