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
    /// Pinned entries sort to the top and are never dropped by the history cap.
    #[serde(default)]
    pub pinned: bool,
}

/// What sort of collection a history entry points at, for labelling the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisitKind {
    Repo,
    Album,
    Search,
}

impl VisitKind {
    pub fn label(self) -> &'static str {
        match self {
            VisitKind::Repo => "repo",
            VisitKind::Album => "album",
            VisitKind::Search => "search",
        }
    }
}

impl Visit {
    /// Inferred from the input, using the same precedence the resolver uses, so
    /// the tag always matches where the entry will actually be loaded from.
    pub fn kind(&self) -> VisitKind {
        if self.input.contains("imgur.com") {
            VisitKind::Album
        } else if crate::RepoRef::parse(&self.input).is_ok() {
            VisitKind::Repo
        } else {
            VisitKind::Search
        }
    }

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

    pub fn record(&mut self, mut visit: Visit) {
        // Revisiting must not silently unpin something.
        if let Some(old) = self.history.iter().find(|v| v.same_place(&visit)) {
            visit.pinned = old.pinned;
        }
        self.history.retain(|v| !v.same_place(&visit));
        self.history.insert(0, visit);
        self.reorder();
        self.trim();
    }

    pub fn forget(&mut self, index: usize) {
        if index < self.history.len() {
            self.history.remove(index);
        }
    }

    /// Returns the new state: true if it is now pinned.
    pub fn toggle_pin(&mut self, index: usize) -> bool {
        let now = match self.history.get_mut(index) {
            Some(v) => {
                v.pinned = !v.pinned;
                v.pinned
            }
            None => return false,
        };
        self.reorder();
        now
    }

    /// Pinned first, each group keeping its own recency order. Stored in this
    /// order rather than sorted on read, so the indices the UI hands back to
    /// `forget` and `toggle_pin` always line up with what was drawn.
    fn reorder(&mut self) {
        let (mut pinned, unpinned): (Vec<Visit>, Vec<Visit>) =
            self.history.drain(..).partition(|v| v.pinned);
        pinned.extend(unpinned);
        self.history = pinned;
    }

    /// Drop the oldest unpinned entries. Pinning is an explicit request to keep
    /// something, so a pinned entry is never discarded — even past the cap.
    fn trim(&mut self) {
        let pinned = self.history.iter().filter(|v| v.pinned).count();
        let room = MAX_HISTORY.saturating_sub(pinned);
        let mut kept_unpinned = 0;
        self.history.retain(|v| {
            if v.pinned {
                return true;
            }
            kept_unpinned += 1;
            kept_unpinned <= room
        });
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
            pinned: false,
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
    fn kind_matches_where_the_entry_will_be_loaded_from() {
        let of = |input: &str| Visit {
            input: input.into(),
            title: String::new(),
            images: 0,
            last_used: 0,
            pinned: false,
        }
        .kind();

        assert_eq!(of("https://github.com/o/r/tree/main/images"), VisitKind::Repo);
        assert_eq!(of("owner/repo"), VisitKind::Repo);
        assert_eq!(of("https://imgur.com/gallery/dump-1Ur1STy"), VisitKind::Album);
        assert_eq!(of("world of warcraft"), VisitKind::Search);
        assert_eq!(of("illidan"), VisitKind::Search);
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
    fn pinned_entries_sort_first_and_survive_revisiting() {
        let dir = tmp_dir("pin");
        let mut lib = Library::open(dir.clone());

        lib.record(visit("a", "one", None));
        lib.record(visit("b", "two", None));
        lib.record(visit("c", "three", None));

        // Pin the oldest, which is now last.
        let last = lib.history().len() - 1;
        assert!(lib.toggle_pin(last), "toggling an unpinned entry pins it");
        assert!(lib.history()[0].pinned, "pinned sorts to the top");
        assert!(lib.history()[0].input.contains("one"));

        // Revisiting it must not clear the pin.
        lib.record(visit("a", "one", None));
        assert!(lib.history()[0].pinned, "revisiting must preserve the pin");

        // And visiting something else must not either.
        lib.record(visit("d", "four", None));
        assert!(lib.history()[0].pinned);
        assert!(lib.history()[0].input.contains("one"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cap_never_discards_a_pinned_entry() {
        let dir = tmp_dir("pincap");
        let mut lib = Library::open(dir.clone());

        lib.record(visit("keep", "me", None));
        assert!(lib.toggle_pin(0));

        // Flood well past the cap.
        for i in 0..MAX_HISTORY * 2 {
            lib.record(visit("o", &format!("r{i}"), None));
        }

        assert!(
            lib.history().iter().any(|v| v.input.contains("keep")),
            "a pinned entry must outlive the cap"
        );
        assert!(lib.history()[0].pinned, "and still lead");
        assert!(lib.history().len() <= MAX_HISTORY);

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
