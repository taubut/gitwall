#!/usr/bin/env python3
"""Build a design harness for the picker.

Runs the real `main.js` and `styles.css` against a stubbed Tauri bridge backed
by whatever thumbnails are already in the cache, so the visuals can be checked
in a headless browser without launching the app or touching the desktop.

    python3 tools/harness.py && \
      firefox --headless --window-size=2560,1440 \
        --screenshot /tmp/shot.png src/_harness.html

The stub resolves every call as an already-settled promise, so the whole render
completes in microtasks — well before the browser fires `load` and the
screenshot is taken.
"""

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
CACHE = Path.home() / ".cache" / "gitwall" / "thumbs"

# Which slice is focused in the captured frame. Far enough in that slices show
# on both sides.
FOCUS = 20


def load_thumbs():
    out = []
    for jpg in sorted(CACHE.rglob("*.jpg")):
        meta_path = jpg.with_suffix(".json")
        if not meta_path.exists():
            continue
        meta = json.loads(meta_path.read_text())
        out.append(
            {
                "file": str(jpg),
                "width": meta["src_w"],
                "height": meta["src_h"],
                "accent": "#%02x%02x%02x" % tuple(meta["accent"]),
            }
        )
    return out


def main() -> None:
    thumbs = load_thumbs()
    if not thumbs:
        raise SystemExit(
            "no cached thumbnails — run the smoke example first:\n"
            "  cargo run -p gitwall-core --example smoke -- <repo> 44"
        )

    # Enough slices that the strip runs past both edges of the screen.
    count = max(60, len(thumbs))
    names = [
        "serial_experiments_lain", "4k-ai-mountain", "nord-forest", "kanagawa-dusk",
        "tokyo-rain", "gruvbox-dunes", "catppuccin-shore", "everforest-pass",
        "moonlit-pier", "static-bloom", "cassette-drift", "long-exposure-01",
    ]
    images = [
        {
            "name": names[i % len(names)] if i % 3 else f"{names[i % len(names)]}_{i:03d}",
            "path": f"images/{names[i % len(names)]}_{i:03d}.png",
            "dir": "images",
            "size": 900_000 + (i * 137_000) % 3_400_000,
        }
        for i in range(count)
    ]

    repo = {
        "display": "D3Ext/aesthetic-wallpapers@main/images",
        "owner": "D3Ext",
        "repo": "aesthetic-wallpapers",
        "commit": "e34f05935c5b0a1d",
        "shortCommit": "e34f059",
        "truncated": False,
        "totalBytes": 683_000_000,
        "images": images,
    }

    stub = f"""
<script>
const REPO = {json.dumps(repo)};
const THUMBS = {json.dumps(thumbs)};

window.__TAURI__ = {{
  core: {{
    invoke(cmd, args) {{
      if (cmd === "resolve_repo") return Promise.resolve(REPO);
      if (cmd === "backend_info")
        return Promise.resolve({{ detected: "gnome", problem: null, backends: [] }});
      if (cmd === "apply_wallpaper") return Promise.resolve("/tmp/wall.png");
      if (cmd === "load_full") {{
        // No full-resolution originals in the cache during design review, so
        // the backdrop stays at thumbnail quality here. The real app fetches
        // the actual full-size image.
        return Promise.resolve(THUMBS[args.index % THUMBS.length].file);
      }}
      if (cmd === "load_thumb") {{
        const t = THUMBS[args.index % THUMBS.length];
        return Promise.resolve(Object.assign({{ index: args.index }}, t));
      }}
      return Promise.resolve(null);
    }},
    convertFileSrc: (p) => "file://" + p,
  }},
  window: {{ getCurrentWindow: () => ({{ close() {{}} }}) }},
}};
</script>
"""

    driver = f"""
<script>
/* `loadRepo` and `focusOn` are top-level declarations in main.js, which is a
   classic script — so they land in the shared global scope and are reachable
   from here. Both hops are microtasks, so this settles before `load`. */
loadRepo("github.com/D3Ext/aesthetic-wallpapers").then(() => {{
  focusOn({FOCUS});
  if (!location.search.includes("debug")) return;
  const p = state.panels[{FOCUS}];
  const r = p && p.getBoundingClientRect();
  const box = document.createElement("pre");
  box.style.cssText =
    "position:fixed;top:70px;left:24px;z-index:9999;color:#7fe;background:#000d;" +
    "font:12px monospace;padding:14px;white-space:pre;line-height:1.5";
  box.textContent = JSON.stringify({{
    screen: document.body.dataset.state,
    panels: state.panels.length,
    stage: [el.stage.clientWidth, el.stage.clientHeight],
    strip: [el.strip.clientWidth, el.strip.clientHeight],
    geo: geo,
    focusPanel: p && {{
      width: p.style.width,
      transform: p.style.transform,
      opacity: p.style.opacity,
      visibility: p.style.visibility,
      zIndex: p.style.zIndex,
      rect: r && [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)],
      computedDisplay: getComputedStyle(p).display,
      innerRect: (() => {{
        const q = p.querySelector(".panel__inner").getBoundingClientRect();
        return [Math.round(q.x), Math.round(q.y), Math.round(q.width), Math.round(q.height)];
      }})(),
      imgSrc: (p.querySelector("img").src || "").slice(-40),
      imgComplete: p.querySelector("img").complete,
    }},
  }}, null, 1);
  document.body.appendChild(box);
}});
</script>
"""

    # Screenshots are taken at `load`, which lands mid-transition. Freeze all
    # motion so the capture shows the settled layout.
    freeze = (
        "<style>*,*::before,*::after{transition:none!important;"
        "animation:none!important}</style>"
    )

    html = (SRC / "index.html").read_text()
    html = html.replace("</head>", freeze + "</head>")
    html = html.replace('<script src="main.js"></script>', stub + '<script src="main.js"></script>' + driver)
    # The harness must never be mistaken for the shipped entry point.
    html = html.replace("<title>gitwall</title>", "<title>gitwall — design harness</title>")

    out = SRC / "_harness.html"
    out.write_text(html)
    print(f"wrote {out}  ({len(thumbs)} cached thumbnails, {count} slices, focus {FOCUS})")


if __name__ == "__main__":
    main()
