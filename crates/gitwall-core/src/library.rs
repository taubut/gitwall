//! Persisted user state: starred wallpapers and recently browsed repos.
//!
//! Lives in the data directory, not the cache, so `--clear-cache` or a tmp
//! reaper can never lose it. Both files are tolerated missing or corrupt — a
//! damaged favourites file should cost you your stars, not the whole app.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::cache::write_atomic;
use crate::Result;

const MAX_HISTORY: usize = 24;

/// A starred wallpaper.
///
/// Stores its own URLs rather than "owner/repo/commit/path", which is what lets
/// one favourites list span GitHub repos *and* Imgur albums — and lets it keep
/// working when a new source is added later. GitHub URLs embed the commit, so
/// the old pinning guarantee survives: a starred wallpaper keeps resolving even
/// if the branch moves.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Favourite {
    /// Cache key — git blob sha, or the Imgur id.
    pub key: String,
    pub name: String,
    /// Repo-relative path, or the Imgur id. Display and error messages.
    pub label: String,
    /// Section it came from, if any.
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Where it came from, for display: `owner/repo` or `imgur/<hash>`.
    #[serde(default)]
    pub origin: String,
    /// Browsing-sized image. On Imgur this is a native thumbnail; on GitHub it
    /// is the original, which is all that's on offer.
    pub preview: [String; 2],
    pub full: [String; 2],
    #[serde(default)]
    pub added: u64,
}

/// A collection the user has opened before — a GitHub repo, an Imgur album, or
/// whatever gets added next.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Visit {
    /// Exactly what to feed back in to reopen it.
    pub input: String,
    /// Human label: `owner/repo · subdir`, or an album title.
    ///
    /// Defaulted so that adding fields later degrades to a usable entry rather
    /// than failing the whole file and silently wiping someone's history.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub images: usize,
    #[serde(default)]
    pub last_used: u64,
}

impl Visit {
    pub fn label(&self) -> String {
        if self.title.is_empty() {
            self.input.clone()
        } else {
            self.title.clone()
        }
    }

    /// Keyed on the input, so `.../tree/main/images` and `.../tree/main` stay
    /// separate entries — they really are different collections.
    fn same_place(&self, other: &Visit) -> bool {
        self.input == other.input
    }
}

pub struct Library {
    dir: PathBuf,
    favourites: Vec<Favourite>,
    history: Vec<Visit>,
}

impl Library {
    /// Never fails: unreadable or malformed state starts empty rather than
    /// blocking the app.
    pub fn open(dir: PathBuf) -> Self {
        let read = |name: &str| -> Option<Vec<u8>> { std::fs::read(dir.join(name)).ok() };

        let favourites = read("favourites.json")
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let history = read("history.json")
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();

        Self {
            dir,
            favourites,
            history,
        }
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| crate::Error::io(&self.dir, e))?;
        write_atomic(
            &self.dir.join("favourites.json"),
            &serde_json::to_vec_pretty(&self.favourites)?,
        )?;
        write_atomic(
            &self.dir.join("history.json"),
            &serde_json::to_vec_pretty(&self.history)?,
        )?;
        Ok(())
    }

    /* --------------------------------------------------------- favourites */

    /// Newest star first.
    pub fn favourites(&self) -> &[Favourite] {
        &self.favourites
    }

    /// Keyed by content, so the same wallpaper vendored into two repos is one
    /// star, not two.
    pub fn is_favourite(&self, key: &str) -> bool {
        self.favourites.iter().any(|f| f.key == key)
    }

    /// Returns the new state: true if it is now starred.
    pub fn toggle_favourite(&mut self, fav: Favourite) -> bool {
        if let Some(i) = self.favourites.iter().position(|f| f.key == fav.key) {
            self.favourites.remove(i);
            false
        } else {
            self.favourites.insert(0, fav);
            true
        }
    }

    /// Distinct sources represented in the favourites.
    pub fn favourite_origins(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for f in &self.favourites {
            if !seen.contains(&f.origin) {
                seen.push(f.origin.clone());
            }
        }
        seen
    }

    /* ------------------------------------------------------------ history */

    /// Most recent first.
    pub fn history(&self) -> &[Visit] {
        &self.history
    }

    pub fn record(&mut self, visit: Visit) {
        self.history.retain(|v| !v.same_place(&visit));
        self.history.insert(0, visit);
        self.history.truncate(MAX_HISTORY);
    }

    pub fn forget(&mut self, index: usize) {
        if index < self.history.len() {
            self.history.remove(index);
        }
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("gitwall-lib-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn fav(key: &str) -> Favourite {
        Favourite {
            key: key.into(),
            name: key.into(),
            label: format!("images/{key}.png"),
            group: "images".into(),
            ext: "PNG".into(),
            size: 1,
            width: 0,
            height: 0,
            origin: "o/r".into(),
            preview: ["p1".into(), "p2".into()],
            full: ["f1".into(), "f2".into()],
            added: 0,
        }
    }

    fn visit(owner: &str, repo: &str, subdir: Option<&str>) -> Visit {
        let input = match subdir {
            Some(d) => format!("{owner}/{repo}/tree/main/{d}"),
            None => format!("{owner}/{repo}"),
        };
        Visit {
            title: input.clone(),
            input,
            images: 3,
            last_used: 0,
        }
    }

    #[test]
    fn starring_twice_unstars() {
        let dir = tmp_dir("toggle");
        let mut lib = Library::open(dir.clone());

        assert!(!lib.is_favourite("abc"));
        assert!(lib.toggle_favourite(fav("abc")), "first toggle stars it");
        assert!(lib.is_favourite("abc"));
        assert!(!lib.toggle_favourite(fav("abc")), "second toggle unstars it");
        assert!(!lib.is_favourite("abc"));
        assert!(lib.favourites().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn favourites_and_history_survive_a_reload() {
        let dir = tmp_dir("persist");
        {
            let mut lib = Library::open(dir.clone());
            lib.toggle_favourite(fav("one"));
            lib.toggle_favourite(fav("two"));
            lib.record(visit("o", "r", None));
            lib.save().unwrap();
        }

        let lib = Library::open(dir.clone());
        assert_eq!(lib.favourites().len(), 2);
        assert!(lib.is_favourite("one") && lib.is_favourite("two"));
        assert_eq!(lib.favourites()[0].key, "two", "newest star leads");
        assert_eq!(lib.history().len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_history_entry_missing_its_title_still_loads() {
        let dir = tmp_dir("older");
        std::fs::write(
            dir.join("history.json"),
            br#"[{"input":"o/r","images":5,"last_used":1}]"#,
        )
        .unwrap();

        let lib = Library::open(dir.clone());
        assert_eq!(lib.history().len(), 1, "must not discard the whole file");
        assert_eq!(lib.history()[0].label(), "o/r", "falls back to the input");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_state_starts_empty_instead_of_failing() {
        let dir = tmp_dir("corrupt");
        std::fs::write(dir.join("favourites.json"), b"{not json").unwrap();
        std::fs::write(dir.join("history.json"), b"\x00\x01").unwrap();

        let lib = Library::open(dir.clone());
        assert!(lib.favourites().is_empty());
        assert!(lib.history().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn revisiting_a_repo_moves_it_to_the_front_without_duplicating() {
        let dir = tmp_dir("hist");
        let mut lib = Library::open(dir.clone());

        lib.record(visit("a", "one", None));
        lib.record(visit("b", "two", None));
        lib.record(visit("a", "one", None));

        assert_eq!(lib.history().len(), 2, "no duplicate entry");
        assert!(lib.history()[0].input.contains("one"), "most recent leads");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn different_subdirectories_of_one_repo_are_separate_entries() {
        let dir = tmp_dir("subdir");
        let mut lib = Library::open(dir.clone());

        lib.record(visit("a", "one", Some("images")));
        lib.record(visit("a", "one", Some("walls")));
        lib.record(visit("a", "one", None));

        assert_eq!(lib.history().len(), 3);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn history_is_capped() {
        let dir = tmp_dir("cap");
        let mut lib = Library::open(dir.clone());
        for i in 0..MAX_HISTORY + 10 {
            lib.record(visit("o", &format!("r{i}"), None));
        }
        assert_eq!(lib.history().len(), MAX_HISTORY);
        assert!(
            lib.history()[0].input.contains(&format!("r{}", MAX_HISTORY + 9)),
            "newest survives"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn favourite_origins_lists_each_source_once() {
        let dir = tmp_dir("origins");
        let mut lib = Library::open(dir.clone());

        // Two GitHub repos and one Imgur album — a favourites list is allowed
        // to span sources entirely.
        let mut a = fav("x");
        a.origin = "o/alpha".into();
        let mut b = fav("y");
        b.origin = "o/alpha".into();
        let mut c = fav("z");
        c.origin = "imgur/1Ur1STy".into();

        lib.toggle_favourite(a);
        lib.toggle_favourite(b);
        lib.toggle_favourite(c);

        assert_eq!(lib.favourite_origins().len(), 2);

        let _ = std::fs::remove_dir_all(dir);
    }
}
