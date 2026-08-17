/* gitwall — slice carousel over a GitHub repo.
 *
 * Geometry follows skwd-wall's "slices" presentation: a row of sheared,
 * overlapping slices with one expanded to full width. Two details there are
 * easy to get wrong and both matter to the look:
 *
 *   - the gap is NEGATIVE, so slices overlap and read as a stack of cards
 *   - the lean is a fixed pixel offset over the slice height, not a fixed
 *     angle, so every dimension derives from one number
 */

const { invoke, convertFileSrc } = window.__TAURI__.core;
const { getCurrentWindow } = window.__TAURI__.window;

/* ----------------------------------------------------------- geometry ---- */

/* Ratios are skwd-wall's "M" preset (slice 432 tall: width 108, expanded 768,
 * gap -30, skew 28) normalised against slice height, so one number scales the
 * whole strip.
 *
 * SKEW is the exception: upstream's M preset works out to 0.065, which is a
 * much gentler lean than the screenshots show. 0.16 matches those. Change this
 * one number to taste — everything else follows. */
const RATIO = {
  /* Upstream's M preset is 0.250, which fits about 7 slices a side on a 16:9
   * screen — they then run off the edge at full opacity. Narrower slices let
   * ~11 fit, so the fade finishes on screen the way the screenshots show. */
  sliceW: 0.18,
  expanded: 768 / 432, //  1.778  (16:9)
  gap: -0.055, // negative on purpose: slices overlap
  /* Upstream's M works out to 0.065, a much gentler lean than the screenshots
   * show. Change this one number to taste — everything else follows. */
  skew: 0.2,
};

/* Fraction of the window the strip occupies. */
const HEIGHT_RATIO = 0.4;
const EXPANDED_MAX_W = 0.4;

/* How many slices stay at full opacity, and how many more fade to nothing
 * past that. Tuned so the outermost on-screen slice is nearly transparent. */
const FULL_ZONE = 4;
const FADE_ZONE = 7;
/* Slices outside this are not worth positioning. */
const RENDER_WINDOW = FULL_ZONE + FADE_ZONE + 2;

/* How far ahead of the focus to pull thumbnails, and how many requests to keep
 * in the air. The Rust side caps real concurrency at 6; this just stops us
 * queueing hundreds of stale requests during a fast scroll. */
const LOOKAHEAD = 14;
const LOOKBEHIND = 8;
const MAX_INFLIGHT = 10;

const geo = {
  sliceH: 432,
  sliceW: 108,
  expanded: 768,
  gap: -30,
  skewPx: 69,
  skewDeg: -9.1,
  /* Cached, never read from the DOM during a scroll. Reading a layout property
   * like clientWidth after writing styles forces a synchronous reflow of the
   * whole document — with a few hundred slices in the tree that alone cost
   * ~50ms per step. The stage spans the viewport, so innerWidth is the same
   * number without touching layout. */
  stageW: 0,
};

function measure() {
  geo.stageW = window.innerWidth;
  const h = Math.round(
    Math.min(Math.max(window.innerHeight * HEIGHT_RATIO, 220), 700)
  );
  geo.sliceH = h;
  geo.sliceW = Math.round(h * RATIO.sliceW);
  geo.gap = Math.round(h * RATIO.gap);
  geo.skewPx = Math.round(h * RATIO.skew);
  geo.skewDeg = -(Math.atan(geo.skewPx / h) * 180) / Math.PI;

  // Expanded width is 16:9 off the slice height, but never so wide it crowds
  // out the neighbours on a short-and-wide window.
  geo.expanded = Math.round(
    Math.min(h * RATIO.expanded, window.innerWidth * EXPANDED_MAX_W)
  );

  const root = document.documentElement.style;
  root.setProperty("--panel-h", `${geo.sliceH}px`);
  root.setProperty("--shear", `${geo.skewDeg}deg`);
  root.setProperty("--skew-comp", `${geo.skewPx}px`);
}

/* -------------------------------------------------------------- state ---- */

const state = {
  repo: null,
  images: [],
  focus: 0,
  /** index -> { status: 'idle'|'loading'|'ready'|'failed', meta } */
  slots: [],
  panels: [],
  queue: new Set(),
  inflight: 0,
  ambientToggle: false,
  ambientEvict: null,
  backdropIndex: -1,
  backdropQuality: 0,
  settleTimer: null,
  settleToken: 0,
  applying: false,
};

const el = {
  body: document.body,
  form: document.getElementById("source-form"),
  input: document.getElementById("repo-input"),
  suggest: document.getElementById("suggest"),
  pinned: document.getElementById("pinned"),
  stage: document.getElementById("stage"),
  strip: document.getElementById("strip"),
  invite: document.getElementById("invite"),
  working: document.getElementById("working"),
  workingText: document.getElementById("working-text"),
  fault: document.getElementById("fault"),
  faultHead: document.getElementById("fault-head"),
  faultBody: document.getElementById("fault-body"),
  detail: document.getElementById("detail"),
  title: document.getElementById("title"),
  specs: document.getElementById("specs"),
  apply: document.getElementById("apply"),
  rail: document.getElementById("rail"),
  railFill: document.getElementById("rail-fill"),
  railCount: document.getElementById("rail-count"),
  toast: document.getElementById("toast"),
  ambientA: document.getElementById("ambient-a"),
  ambientB: document.getElementById("ambient-b"),
};

/* --------------------------------------------------------------- utils ---- */

const bytes = (n) => {
  if (!n) return "—";
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} MB`;
  return `${Math.round(n / 1e3)} KB`;
};

const pad = (n, width) => String(n).padStart(width, "0");

let toastTimer = null;
function toast(message, tone = "good") {
  el.toast.textContent = message;
  el.toast.dataset.tone = tone;
  el.toast.classList.add("is-up");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.toast.classList.remove("is-up"), 2600);
}

function setScreen(name) {
  el.body.dataset.state = name;
  el.invite.hidden = name !== "empty";
  el.working.hidden = name !== "working";
  el.fault.hidden = name !== "fault";
  el.detail.hidden = name !== "loaded";
  el.rail.hidden = name !== "loaded";
}

/* --------------------------------------------------------------- layout --- */

/* Analytic, so this stays cheap no matter how many images the repo has:
 * every slice before the focus sits on a fixed step, and everything after it
 * is shifted by the extra width the focused slice takes up. */
function xFor(index) {
  const step = geo.sliceW + geo.gap;
  const base = index * step;
  return index > state.focus ? base + (geo.expanded - geo.sliceW) : base;
}

function opacityFor(distance) {
  if (distance <= FULL_ZONE) return 1;
  return Math.max(0, 1 - (distance - FULL_ZONE) / FADE_ZONE);
}

function layout({ animate = true } = {}) {
  if (!state.images.length) return;

  // Freshly built slices have no width yet, so without this they would animate
  // in from zero. Suppress transitions for exactly one frame instead.
  if (!animate) el.strip.classList.add("is-still");

  const focusCentre = xFor(state.focus) + geo.expanded / 2;
  const offset = geo.stageW / 2 - focusCentre;

  const lo = Math.max(0, state.focus - RENDER_WINDOW);
  const hi = Math.min(state.images.length - 1, state.focus + RENDER_WINDOW);

  for (let i = 0; i < state.panels.length; i++) {
    const panel = state.panels[i];
    if (i < lo || i > hi) {
      // Park it rather than restyling: offscreen slices cost nothing.
      if (panel.dataset.parked !== "1") {
        panel.dataset.parked = "1";
        panel.style.opacity = "0";
        panel.style.visibility = "hidden";
      }
      continue;
    }

    const distance = Math.abs(i - state.focus);
    const focused = i === state.focus;

    panel.dataset.parked = "0";
    panel.style.visibility = "visible";
    panel.style.opacity = String(opacityFor(distance));
    panel.style.width = `${focused ? geo.expanded : geo.sliceW}px`;
    panel.style.transform = `translateX(${xFor(i) + offset}px) skewX(var(--shear))`;
    // Stack inward so each slice tucks under the one nearer the focus.
    panel.style.zIndex = String(1000 - distance);
    panel.classList.toggle("is-focused", focused);
    panel.setAttribute("aria-selected", focused ? "true" : "false");
  }

  if (!animate) {
    void el.strip.offsetWidth; // flush the suppressed styles before re-enabling
    el.strip.classList.remove("is-still");
  }
}

/* ------------------------------------------------------------ thumbnails -- */

function markPanel(index) {
  const panel = state.panels[index];
  const slot = state.slots[index];
  if (!panel || !slot) return;
  panel.classList.toggle("panel--pending", slot.status !== "ready" && slot.status !== "failed");
  panel.classList.toggle("panel--failed", slot.status === "failed");
}

function pump() {
  while (state.inflight < MAX_INFLIGHT && state.queue.size) {
    // Always take whatever is closest to where the user is looking, so a fast
    // scroll re-prioritises instead of draining a stale queue.
    let best = null;
    let bestDistance = Infinity;
    for (const i of state.queue) {
      const d = Math.abs(i - state.focus);
      if (d < bestDistance) {
        bestDistance = d;
        best = i;
      }
    }
    if (best === null) break;

    state.queue.delete(best);
    fetchThumb(best);
  }
}

async function fetchThumb(index) {
  const slot = state.slots[index];
  if (!slot || slot.status !== "idle") return;

  slot.status = "loading";
  state.inflight++;

  try {
    const dto = await invoke("load_thumb", { index });
    slot.status = "ready";
    slot.meta = dto;

    const panel = state.panels[index];
    const img = panel?.querySelector("img");
    if (img) {
      img.onload = () => img.classList.add("is-in");
      img.src = convertFileSrc(dto.file);
      img.alt = state.images[index].name;
    }
    markPanel(index);

    // The focused slice may have been waiting on this to colour the UI.
    if (index === state.focus) paintFocus();
  } catch (err) {
    // One bad image must never take down the gallery.
    slot.status = "failed";
    slot.error = String(err);
    markPanel(index);
    if (index === state.focus) paintFocus();
  } finally {
    state.inflight--;
    pump();
  }
}

function requestAround(centre) {
  const lo = Math.max(0, centre - LOOKBEHIND);
  const hi = Math.min(state.images.length - 1, centre + LOOKAHEAD);
  for (let i = lo; i <= hi; i++) {
    if (state.slots[i].status === "idle") state.queue.add(i);
  }
  pump();
}

/* ----------------------------------------------------------------- focus -- */

/* The backdrop is upgraded in two steps: the cached thumbnail goes up
 * immediately so the screen reacts the instant you scroll, then the real
 * full-resolution image replaces it once you stop. Browsing 372 wallpapers
 * still only downloads thumbnails; the ones you actually linger on get sharp. */
const QUALITY_THUMB = 1;
const QUALITY_FULL = 2;
const SETTLE_MS = 320;

function crossfade(url) {
  const [show, hide] = state.ambientToggle
    ? [el.ambientA, el.ambientB]
    : [el.ambientB, el.ambientA];
  state.ambientToggle = !state.ambientToggle;

  if (!url) {
    show.classList.remove("is-live");
    hide.classList.remove("is-live");
    return;
  }

  show.style.backgroundImage = `url("${url}")`;
  show.classList.add("is-live");
  hide.classList.remove("is-live");

  // Drop the outgoing image once it has faded out — these are full-resolution
  // 4K frames and holding both decoded is a lot of memory for no benefit.
  clearTimeout(state.ambientEvict);
  state.ambientEvict = setTimeout(() => {
    if (!hide.classList.contains("is-live")) hide.style.backgroundImage = "";
  }, 700);
}

function setBackdrop(index, url, quality) {
  // Never downgrade a backdrop that is already showing at full resolution.
  if (state.backdropIndex === index && state.backdropQuality >= quality) return;
  state.backdropIndex = index;
  state.backdropQuality = quality;
  crossfade(url);
}

function clearBackdrop() {
  clearTimeout(state.settleTimer);
  state.settleToken++;
  state.backdropIndex = -1;
  state.backdropQuality = 0;
  crossfade(null);
}

function scheduleFullBackdrop() {
  clearTimeout(state.settleTimer);
  const token = ++state.settleToken;
  const index = state.focus;

  state.settleTimer = setTimeout(async () => {
    try {
      const file = await invoke("load_full", { index });
      // Bail if the user kept scrolling while this was downloading.
      if (token !== state.settleToken || index !== state.focus) return;
      setBackdrop(index, convertFileSrc(file), QUALITY_FULL);
    } catch {
      /* keep the thumbnail backdrop; the apply path reports real failures */
    }
  }, SETTLE_MS);
}

function paintFocus() {
  const image = state.images[state.focus];
  if (!image) return;
  const slot = state.slots[state.focus];

  el.title.textContent = image.name;

  const parts = [];
  if (image.dir) parts.push(`<em>${image.dir}/</em>`);
  if (slot?.meta) parts.push(`${slot.meta.width}&times;${slot.meta.height}`);
  parts.push(bytes(image.size));
  const ext = image.path.split(".").pop();
  if (ext) parts.push(ext.toUpperCase());
  if (slot?.status === "failed") parts.push("<em>could not load</em>");
  el.specs.innerHTML = parts.join(" &middot; ");

  if (slot?.meta) {
    document.documentElement.style.setProperty("--accent", slot.meta.accent);
    setBackdrop(state.focus, convertFileSrc(slot.meta.file), QUALITY_THUMB);
    scheduleFullBackdrop();
  }

  const n = state.images.length;
  el.railFill.style.width = `${((state.focus + 1) / n) * 100}%`;
  el.railCount.innerHTML = `<b>${pad(state.focus + 1, String(n).length)}</b> / ${n}`;
}

function focusOn(index, { smooth = true } = {}) {
  const n = state.images.length;
  if (!n) return;
  const next = Math.min(Math.max(index, 0), n - 1);
  if (next === state.focus && smooth) return;

  state.focus = next;
  layout();
  paintFocus();
  if (!window.__noFetch) requestAround(next);

  const panel = state.panels[next];
  if (panel) panel.tabIndex = 0;
}

/* ------------------------------------------------------------- building --- */

function buildStrip() {
  el.strip.textContent = "";
  const frag = document.createDocumentFragment();

  state.panels = state.images.map((image, i) => {
    const panel = document.createElement("div");
    panel.className = "panel panel--pending";
    panel.setAttribute("role", "option");
    panel.setAttribute("aria-label", image.name);
    panel.tabIndex = -1;
    panel.dataset.index = String(i);
    panel.style.visibility = "hidden";
    panel.style.opacity = "0";

    const inner = document.createElement("div");
    inner.className = "panel__inner";
    const img = document.createElement("img");
    img.alt = "";
    img.draggable = false;
    inner.appendChild(img);
    panel.appendChild(inner);

    frag.appendChild(panel);
    return panel;
  });

  el.strip.appendChild(frag);
}

/* ---------------------------------------------------------------- repo ---- */

async function loadRepo(url) {
  const target = url.trim();
  if (!target) return;

  setScreen("working");
  el.workingText.textContent = "Reading the tree…";
  clearBackdrop();
  document.documentElement.style.setProperty("--accent", "#7fd4e8");

  try {
    const repo = await invoke("resolve_repo", { url: target });

    state.repo = repo;
    state.images = repo.images;
    state.slots = repo.images.map(() => ({ status: "idle", meta: null }));
    state.queue.clear();
    state.focus = 0;

    el.input.value = `github.com/${repo.owner}/${repo.repo}`;
    el.pinned.innerHTML =
      `<b>${repo.shortCommit}</b> &middot; ${repo.images.length} images ` +
      `&middot; ${bytes(repo.totalBytes)}`;

    buildStrip();
    setScreen("loaded");
    measure();
    layout({ animate: false });
    paintFocus();
    requestAround(0);

    if (repo.truncated) {
      toast("GitHub truncated the tree — some images are missing", "bad");
    }
  } catch (err) {
    setScreen("fault");
    const message = String(err);
    el.faultHead.textContent = message.includes("rate limit")
      ? "GitHub is rate limiting."
      : "That didn't load.";
    el.faultBody.textContent = message;
  }
}

/* --------------------------------------------------------------- apply ---- */

async function applyFocused() {
  if (state.applying || el.body.dataset.state !== "loaded") return;

  const image = state.images[state.focus];
  if (!image) return;

  state.applying = true;
  el.apply.disabled = true;
  const label = el.apply.textContent;
  el.apply.textContent = "Setting…";

  try {
    await invoke("apply_wallpaper", { index: state.focus, backend: null });
    toast(`Wallpaper set — ${image.name}`);
  } catch (err) {
    toast(String(err), "bad");
  } finally {
    state.applying = false;
    el.apply.disabled = false;
    el.apply.textContent = label;
  }
}

/* --------------------------------------------------------------- input ---- */

const typing = () => document.activeElement === el.input;

el.form.addEventListener("submit", (e) => {
  e.preventDefault();
  el.input.blur();
  loadRepo(el.input.value);
});

el.suggest.addEventListener("click", () => {
  const repo = "https://github.com/D3Ext/aesthetic-wallpapers/tree/main/images";
  el.input.value = repo;
  loadRepo(repo);
});

el.apply.addEventListener("click", applyFocused);

el.stage.addEventListener("click", (e) => {
  const panel = e.target.closest(".panel");
  if (!panel) return;
  const index = Number(panel.dataset.index);
  // Click to bring into focus; click the focused one to commit it.
  if (index === state.focus) applyFocused();
  else focusOn(index);
});

// Trackpads deliver both axes; take whichever is larger so a vertical flick
// scrolls the strip too.
let wheelAccum = 0;
el.stage.addEventListener(
  "wheel",
  (e) => {
    e.preventDefault();
    if (el.body.dataset.state !== "loaded") return;

    const delta =
      Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
    wheelAccum += delta;

    const threshold = 40;
    while (Math.abs(wheelAccum) >= threshold) {
      const step = Math.sign(wheelAccum);
      wheelAccum -= step * threshold;
      focusOn(state.focus + step);
    }
  },
  { passive: false }
);

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    if (typing()) {
      el.input.blur();
      return;
    }
    getCurrentWindow().close();
    return;
  }

  if (e.key === "/" && !typing()) {
    e.preventDefault();
    el.input.focus();
    el.input.select();
    return;
  }

  if (typing()) return;
  if (el.body.dataset.state !== "loaded") return;

  const jumps = {
    ArrowLeft: -1,
    ArrowRight: 1,
    PageUp: -10,
    PageDown: 10,
  };

  if (e.key in jumps) {
    e.preventDefault();
    focusOn(state.focus + jumps[e.key]);
  } else if (e.key === "Home") {
    e.preventDefault();
    focusOn(0);
  } else if (e.key === "End") {
    e.preventDefault();
    focusOn(state.images.length - 1);
  } else if (e.key === "Enter") {
    e.preventDefault();
    applyFocused();
  }
});

let resizeTimer = null;
window.addEventListener("resize", () => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => {
    measure();
    layout({ animate: false });
  }, 80);
});

/* ----------------------------------------------------------------- perf ---- */

/* Steps the focus on every animation frame — the worst case a user can
 * produce by holding an arrow key — and reports the frame-time distribution.
 * Enabled with GITWALL_PERF=1; never runs otherwise. */
async function perfProbe() {
  window.__noFetch = true;
  const STEPS = 90;
  const TAIL = 60;
  const frames = [];

  await new Promise((resolve) => {
    let last = performance.now();
    let steps = 0;
    requestAnimationFrame(function tick(now) {
      frames.push(now - last);
      last = now;
      if (steps < STEPS) {
        focusOn(state.focus + 1);
        steps++;
        requestAnimationFrame(tick);
      } else if (frames.length < STEPS + TAIL) {
        requestAnimationFrame(tick); // let the last transition settle
      } else {
        resolve();
      }
    });
  });

  // Drop the first frame: it carries the delta from before the probe started.
  const d = frames.slice(1).sort((a, b) => a - b);
  const at = (q) => d[Math.min(d.length - 1, Math.floor(d.length * q))];
  const mean = d.reduce((a, b) => a + b, 0) / d.length;

  const summary = {
    frames: d.length,
    fps_mean: +(1000 / mean).toFixed(1),
    ms_p50: +at(0.5).toFixed(2),
    ms_p95: +at(0.95).toFixed(2),
    ms_worst: +d[d.length - 1].toFixed(2),
    janky_over_20ms: d.filter((x) => x > 20).length,
  };

  await invoke("report_perf", { summary: JSON.stringify(summary) });
}

/* ---------------------------------------------------------------- boot ---- */

async function boot() {
  measure();
  setScreen("empty");

  // Surface a missing wallpaper tool now rather than at the moment of failure.
  try {
    const info = await invoke("backend_info");
    if (!info.detected && info.problem) toast(info.problem, "bad");
  } catch {
    /* non-fatal: the apply path reports its own errors */
  }

  // `gitwall github.com/owner/repo` opens straight into that repo.
  try {
    const initial = await invoke("initial_repo");
    if (initial) {
      el.input.value = initial;
      await loadRepo(initial);
      if (await invoke("perf_mode")) await perfProbe();
      return;
    }
  } catch {
    /* fall through to the empty state */
  }

  el.input.focus();
}

boot();
