//! gitwall — browse a GitHub repo of wallpapers and set one.
//!
//! Renders natively through wgpu (Vulkan here). There is deliberately no
//! webview: WebKitGTK's compositing path is unusable on this hardware, which
//! capped the previous build around 14 fps no matter what was removed from it.

mod app;
mod bridge;
mod geom;
mod theme;

use gitwall_core::Cache;

fn main() -> eframe::Result<()> {
    let cache = match Cache::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gitwall: {e}");
            std::process::exit(1);
        }
    };

    // `gitwall github.com/owner/repo` opens straight into that repo.
    let initial = std::env::args().nth(1).filter(|a| !a.starts_with('-'));

    // The picker is fullscreen by default; this drops it into a normal window
    // for development.
    let windowed = std::env::var_os("GITWALL_WINDOWED").is_some();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("gitwall")
            .with_app_id("gitwall")
            .with_decorations(windowed)
            .with_fullscreen(!windowed)
            .with_inner_size([1600.0, 900.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "gitwall",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, cache, initial)))),
    )
}
