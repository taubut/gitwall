//! Live check against a real Imgur album.
//!
//!   cargo run -p gitwall-core --example imgur -- <gallery-url> [count]

use std::time::Instant;

use gitwall_core::imgur::{parse_ref, ImgurClient};
use gitwall_core::{Cache, Fetcher};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .unwrap_or_else(|| "https://imgur.com/gallery/4k-wallpaper-dump-1Ur1STy".into());
    let count: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);

    let reference = parse_ref(&input).ok_or("not an imgur URL")?;
    println!("ref       {reference:?}");

    let http = reqwest::Client::builder()
        .user_agent(concat!("gitwall/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let t = Instant::now();
    let album = ImgurClient::new(http.clone()).resolve(&reference).await?;
    println!("title     {}", album.title);
    println!(
        "images    {} in {:?}  ({:.0} MB full-res, avg {:.2} MB)",
        album.items.len(),
        t.elapsed(),
        album.total_bytes() as f64 / 1e6,
        album.total_bytes() as f64 / album.items.len() as f64 / 1e6
    );

    let cache = Cache::new()?;
    let fetcher = Fetcher::new(http);
    let want = count.min(album.items.len());
    println!("\nfetching {want} previews (Imgur's native thumbnails)...");

    let t = Instant::now();
    let tasks: Vec<_> = album.items[..want]
        .iter()
        .map(|it| {
            let (f, c, it) = (&fetcher, &cache, it.clone());
            async move {
                let urls = [it.preview_url(), it.full_url()];
                let r = f.thumb_at(c, &it.id, &it.full_url(), urls).await;
                (it, r)
            }
        })
        .collect();

    let results = futures::future::join_all(tasks).await;
    let elapsed = t.elapsed();

    let mut ok = 0u32;
    let mut bytes = 0u64;
    for (it, res) in &results {
        match res {
            Ok((p, meta)) => {
                ok += 1;
                let sz = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                bytes += sz;
                println!(
                    "  ok    {:<10} {:>5}x{:<5} {:>6.1} KB cached  (original {:>5.1} MB)  accent #{:02x}{:02x}{:02x}{}",
                    it.id,
                    it.width,
                    it.height,
                    sz as f64 / 1024.0,
                    it.size as f64 / 1e6,
                    meta.accent[0], meta.accent[1], meta.accent[2],
                    if meta.mono { " mono" } else { "" },
                );
            }
            Err(e) => println!("  FAIL  {:<10} {e}", it.id),
        }
    }

    println!("\n{ok}/{want} in {elapsed:?} -> {:.0} KB cached", bytes as f64 / 1024.0);
    let originals: u64 = results.iter().filter(|(_, r)| r.is_ok()).map(|(i, _)| i.size).sum();
    if bytes > 0 {
        println!(
            "browsing the whole album would cost ~{:.0} MB, versus {:.0} MB of originals",
            (bytes as f64 / ok.max(1) as f64) * album.items.len() as f64 / 1e6,
            album.total_bytes() as f64 / 1e6,
        );
        println!("compression {:.0}x", originals as f64 / bytes as f64);
    }

    Ok(())
}
