//! End-to-end check against a real repo, without any UI.
//!
//!   cargo run -p gitwall-core --example smoke -- <repo-url> [count]
//!
//! Resolves the repo, pulls `count` thumbnails concurrently, and reports what
//! the detected wallpaper backend would be. Applies nothing.

use std::time::Instant;

use gitwall_core::{source::GithubClient, Backend, Cache, Fetcher, RepoRef};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .unwrap_or_else(|| "https://github.com/D3Ext/aesthetic-wallpapers/tree/main".into());
    let count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);

    let repo = RepoRef::parse(&input)?;
    println!("repo      {}", repo.display());

    let cache = Cache::new()?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("gitwall/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let t = Instant::now();
    let listing = match cache.load_listing(&repo) {
        Some(l) => {
            println!("listing   {} images (from cache)", l.images.len());
            l
        }
        None => {
            let l = GithubClient::new(http.clone()).resolve(&repo).await?;
            cache.save_listing(&l)?;
            println!(
                "listing   {} images in {:?} (2 API calls)",
                l.images.len(),
                t.elapsed()
            );
            l
        }
    };
    println!("commit    {}", &listing.commit[..12.min(listing.commit.len())]);
    if listing.truncated {
        println!("WARNING   tree was truncated; gallery is incomplete");
    }

    let total: u64 = listing.images.iter().map(|i| i.size).sum();
    println!(
        "payload   {:.0} MB full-res, avg {:.1} MB",
        total as f64 / 1e6,
        total as f64 / listing.images.len() as f64 / 1e6
    );

    let fetcher = Fetcher::new(http);
    let want = count.min(listing.images.len());
    println!("\nfetching {want} thumbnails...");

    let t = Instant::now();
    let tasks: Vec<_> = listing.images[..want]
        .iter()
        .map(|e| {
            let (f, c, l, e) = (&fetcher, &cache, &listing, e.clone());
            async move {
                let r = f.thumb(c, l, &e).await;
                (e, r)
            }
        })
        .collect();

    let results = futures::future::join_all(tasks).await;
    let elapsed = t.elapsed();

    let mut ok = 0u32;
    let mut bytes = 0u64;
    for (entry, res) in &results {
        match res {
            Ok((p, meta)) => {
                ok += 1;
                let sz = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                bytes += sz;
                println!(
                    "  ok    {:<34} {:>6.1} KB  {:>5}x{:<5} accent #{:02x}{:02x}{:02x}",
                    truncate(&entry.name, 34),
                    sz as f64 / 1024.0,
                    meta.src_w,
                    meta.src_h,
                    meta.accent[0],
                    meta.accent[1],
                    meta.accent[2],
                );
            }
            // A repo can contain an image format we can't decode. Skipping one
            // must never take down the gallery.
            Err(e) => println!("  FAIL  {:<44} {e}", truncate(&entry.name, 44)),
        }
    }

    println!(
        "\n{ok}/{want} in {elapsed:?}  ->  {:.0} KB of thumbnails",
        bytes as f64 / 1024.0
    );
    if ok > 0 {
        let src: u64 = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(e, _)| e.size)
            .sum();
        println!("compression {:.0}x vs full-res", src as f64 / bytes.max(1) as f64);
    }

    print!("\nbackend   ");
    match Backend::detect() {
        Ok(b) => println!("{} (detected)", b.name()),
        Err(e) => println!("none — {e}"),
    }
    for (b, avail) in Backend::survey() {
        println!("  {:<11} {}", b.name(), if avail { "available" } else { "-" });
    }

    let s = cache.stats();
    println!(
        "\ncache     {} thumbs ({:.1} MB), {} full ({:.1} MB) in {}",
        s.thumb_count,
        s.thumb_bytes as f64 / 1e6,
        s.full_count,
        s.full_bytes as f64 / 1e6,
        cache.cache_root().display()
    );

    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}
