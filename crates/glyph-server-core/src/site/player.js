// Plays a shared playlist on its /p/ page and in the /embed/p/ player, with
// the app's behaviour: rows play from there, Play all / shuffle, repeat
// (off / all / one), thin bars with a value label, tooltips, the song menu,
// toasts. Song n streams from base + "/listen/" + n (a redirect to a free,
// legal source); songs that fail are skipped.
function glyphPlayer(base, count) {
  const $ = (id) => document.getElementById(id);
  const body = document.body, ds = body.dataset;
  const fmt = (s) => (isFinite(s) ? Math.floor(s / 60) + ":" + String(Math.floor(s % 60)).padStart(2, "0") : "-:--");

  // Toast (toasts.rs): check icon + title, fades in from above.
  function toast(text) {
    const t = $("toast");
    t.querySelector("span").textContent = text;
    t.classList.add("show");
    clearTimeout(t._h);
    t._h = setTimeout(() => t.classList.remove("show"), 2200);
  }
  const copy = (text, msg) => navigator.clipboard.writeText(text).then(() => toast(msg));

  // Tooltips (widgets.rs Tip) for everything with data-tip.
  const tip = $("tip");
  let tipTimer;
  document.addEventListener("pointerover", (e) => {
    const el = e.target.closest("[data-tip]");
    clearTimeout(tipTimer);
    tip.classList.remove("show");
    if (!el || e.pointerType === "touch") return;
    tipTimer = setTimeout(() => {
      tip.textContent = el.dataset.tip;
      const r = el.getBoundingClientRect(), w = tip.offsetWidth, h = tip.offsetHeight;
      let y = r.bottom + 6;
      if (y + h > innerHeight - 4) y = r.top - h - 6;
      tip.style.left = Math.max(4, Math.min(innerWidth - w - 4, r.left + r.width / 2 - w / 2)) + "px";
      tip.style.top = y + "px";
      tip.classList.add("show");
    }, 450);
  });
  document.addEventListener("pointerdown", () => { clearTimeout(tipTimer); tip.classList.remove("show"); });

  const cp = $("cp");
  if (cp) cp.onclick = () => copy(cp.dataset.link, ds.copied);
  if (!count || !$("pp")) return;

  const a = new Audio();
  a.preload = "none";
  const rows = [...document.querySelectorAll(".row[data-n]")];
  const natural = () => rows.map((_, i) => i);
  let order = natural(), at = -1, fails = 0, vol = 1, repeat = 0, shuffled = false;
  const cur = () => (at < 0 ? -1 : order[at]);
  const pop = (el) => { el.classList.remove("pop"); void el.offsetWidth; el.classList.add("pop"); };
  const press = (el) => { el.classList.remove("press"); void el.offsetWidth; el.classList.add("press"); };

  function show(n) {
    const r = rows[n];
    rows.forEach((x) => x.classList.toggle("on", x === r));
    if (!r) return;
    $("nt").textContent = r.dataset.title;
    $("na").textContent = r.dataset.artist;
    $("bar").classList.add("live");
    const img = r.querySelector("img.cv");
    $("ncv").innerHTML = "";
    if (img) {
      const i = new Image();
      i.alt = "";
      i.src = img.src;
      $("ncv").append(i);
    }
    if ("mediaSession" in navigator) {
      navigator.mediaSession.metadata = new MediaMetadata({ title: r.dataset.title, artist: r.dataset.artist, artwork: img ? [{ src: img.src }] : [] });
    }
    const box = r.closest(".elist");
    if (box) {
      const top = r.offsetTop - box.offsetTop;
      if (top < box.scrollTop || top + r.offsetHeight > box.scrollTop + box.clientHeight) box.scrollTo({ top: top - 8, behavior: "smooth" });
    }
  }
  let ticket = 0;
  function start(i) {
    if (i >= order.length && repeat === 1) i = 0;
    if (i < 0 || i >= order.length) return;
    at = i;
    const n = order[i], mine = ++ticket;
    a.pause();
    show(n);
    $("nb").textContent = "";
    $("npv").classList.remove("on");
    // The source and whether it is a preview, like the app's player bar.
    fetch(base + "/listen/" + n + "?json=1")
      .then((r) => (r.ok ? r.json() : Promise.reject()))
      .then((j) => {
        if (mine !== ticket) return;
        $("nb").textContent = "[" + j.badge + "]";
        $("npv").classList.toggle("on", !!j.preview);
        a.src = j.url;
        a.play().catch(() => {});
      })
      .catch(() => { if (mine === ticket) a.onerror(); });
  }
  const playSong = (n) => start(order.indexOf(n));
  function toggle() {
    if (at < 0) return start(0);
    a.paused ? a.play().catch(() => {}) : a.pause();
  }
  function setShuffle(on, from) {
    shuffled = on;
    const keep = cur();
    order = natural();
    if (on) {
      for (let i = order.length - 1; i > 0; i--) { const j = Math.floor(Math.random() * (i + 1)); [order[i], order[j]] = [order[j], order[i]]; }
      if (from == null && keep >= 0) { order.splice(order.indexOf(keep), 1); order.unshift(keep); }
    }
    at = keep >= 0 ? order.indexOf(keep) : -1;
    $("bshuf").classList.toggle("on", on);
    pop($("bshuf"));
  }

  rows.forEach((r) => {
    const go = () => (+r.dataset.n === cur() ? toggle() : playSong(+r.dataset.n));
    r.onclick = (e) => { if (!e.target.closest(".more-btn")) go(); };
    r.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); go(); } };
    r.oncontextmenu = (e) => { e.preventDefault(); menu(r, e.clientX, e.clientY); };
    r.querySelector(".more-btn").onclick = (e) => { e.stopPropagation(); const b = e.currentTarget.getBoundingClientRect(); menu(r, b.left, b.bottom + 4); };
  });
  const all = $("all"), shuf = $("shuf");
  if (all) all.onclick = () => { if (shuffled) setShuffle(false); start(0); };
  if (shuf) shuf.onclick = () => { setShuffle(true, 0); start(0); };
  $("bshuf").onclick = () => setShuffle(!shuffled);
  $("brep").onclick = () => {
    repeat = (repeat + 1) % 3;
    const b = $("brep");
    b.classList.toggle("on", repeat > 0);
    b.querySelector("svg").outerHTML = repeat === 2
      ? '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m17 2 4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="m7 22-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/><path d="M11 10h1v4"/></svg>'
      : '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m17 2 4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="m7 22-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/></svg>';
    b.dataset.tip = repeat === 2 ? ds.repeatOne : ds.repeat;
    pop(b);
  };
  $("pp").onclick = () => { press($("pp").firstElementChild); toggle(); };
  $("nx").onclick = () => { press($("nx")); start(at + 1); };
  $("pv").onclick = () => { press($("pv")); a.currentTime > 3 ? (a.currentTime = 0) : start(Math.max(0, at - 1)); };

  a.onplay = a.onpause = () => body.classList.toggle("playing", !a.paused);
  a.onplaying = () => { fails = 0; };
  a.onended = () => (repeat === 2 ? (a.currentTime = 0, a.play()) : start(at + 1));
  a.onerror = () => {
    if (at < 0) return;
    if (rows[cur()]) rows[cur()].classList.add("off");
    if (++fails < 5 && (at + 1 < order.length || repeat === 1)) start(at + 1);
    else { body.classList.remove("playing"); toast(ds.unplayable); }
  };

  // Thin bars (widgets.rs thin_bar): click or drag; the value shows above.
  function bar(el, set, live, label) {
    const fill = el.querySelector(".f"), knob = el.querySelector(".k"), lbl = el.querySelector(".lbl");
    const draw = (f) => { fill.style.width = f * 100 + "%"; knob.style.left = f * 100 + "%"; };
    const frac = (e) => { const r = el.getBoundingClientRect(); return Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)); };
    const tag = (f) => { lbl.textContent = label(f); lbl.style.left = f * 100 + "%"; };
    el.addEventListener("pointermove", (e) => { if (!el.classList.contains("drag")) tag(frac(e)); });
    el.onpointerdown = (e) => {
      el.setPointerCapture(e.pointerId);
      el.classList.add("drag");
      const f = frac(e);
      draw(f); tag(f);
      if (live) set(f);
      el.onpointermove = (m) => { const g = frac(m); draw(g); tag(g); if (live) set(g); };
      el.onpointerup = (u) => { el.classList.remove("drag"); el.onpointermove = el.onpointerup = null; set(frac(u)); };
    };
    return draw;
  }
  const drawSeek = bar($("seek"), (f) => { if (a.duration) a.currentTime = f * a.duration; }, false, (f) => fmt(f * (a.duration || 0)));
  const muteIcon = (m) => { $("mute").querySelector("svg").outerHTML = m
    ? '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z"/><line x1="22" x2="16" y1="9" y2="15"/><line x1="16" x2="22" y1="9" y2="15"/></svg>'
    : '<svg class="ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z"/><path d="M16 9a5 5 0 0 1 0 6"/><path d="M19.364 18.364a9 9 0 0 0 0-12.728"/></svg>';
    $("mute").dataset.tip = m ? ds.unmute : ds.mute; };
  const drawVol = bar($("vol"), (f) => { vol = f; a.volume = f; a.muted = f <= 0.001; muteIcon(a.muted); }, true, (f) => Math.round(f * 100) + "%");
  $("mute").onclick = () => { a.muted = !a.muted; drawVol(a.muted ? 0 : vol); muteIcon(a.muted); };
  a.ontimeupdate = () => {
    const f = a.duration ? a.currentTime / a.duration : 0;
    if (!$("seek").classList.contains("drag")) drawSeek(f);
    $("mprog").style.width = f * 100 + "%";
    $("t0").textContent = fmt(a.currentTime);
    $("t1").textContent = fmt(a.duration);
  };

  // Song menu (lists.rs ctx_menu): song name, then Play / Copy title / Copy link.
  let open = null;
  function close() {
    if (!open) return;
    const m = open;
    open = null;
    m.classList.remove("show");
    document.querySelectorAll(".row.menu").forEach((r) => r.classList.remove("menu"));
    setTimeout(() => m.remove(), 120);
  }
  function menu(r, x, y) {
    close();
    const m = document.createElement("div");
    m.className = "ctx";
    const hd = document.createElement("div");
    hd.className = "hd trunc";
    hd.textContent = r.dataset.title;
    const sep = document.createElement("div");
    sep.className = "sep";
    m.append(hd, sep);
    const item = (svg, label, fn) => {
      const b = document.createElement("button");
      b.type = "button";
      b.innerHTML = svg;
      const s = document.createElement("span");
      s.className = "trunc";
      s.textContent = label;
      b.append(s);
      b.onclick = () => { close(); fn(); };
      m.append(b);
    };
    const svg = (p, fill) => `<svg class="ic" viewBox="0 0 24 24" fill="${fill ? "currentColor" : "none"}" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${p}</svg>`;
    item(svg('<polygon points="6 3 20 12 6 21 6 3"/>', true), ds.menuPlay, () => playSong(+r.dataset.n));
    item(svg('<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>'), ds.menuCopy, () => copy(r.dataset.title + " — " + r.dataset.artist, ds.copiedSong));
    item(svg('<path d="M9 17H7A5 5 0 0 1 7 7h2"/><path d="M15 7h2a5 5 0 1 1 0 10h-2"/><line x1="8" x2="16" y1="12" y2="12"/>'), ds.menuLink, () => copy(location.origin + base, ds.copied));
    document.body.append(m);
    const w = m.offsetWidth, h = m.offsetHeight;
    m.style.left = Math.max(6, Math.min(innerWidth - w - 6, x)) + "px";
    m.style.top = Math.max(6, Math.min(innerHeight - h - 6, y)) + "px";
    r.classList.add("menu");
    requestAnimationFrame(() => m.classList.add("show"));
    open = m;
  }
  document.addEventListener("pointerdown", (e) => { if (open && !open.contains(e.target) && !e.target.closest(".more-btn")) close(); });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") close();
    if (e.code === "Space" && e.target === document.body) { e.preventDefault(); toggle(); }
  });
  addEventListener("scroll", close, true);

  if ("mediaSession" in navigator) {
    navigator.mediaSession.setActionHandler("play", toggle);
    navigator.mediaSession.setActionHandler("pause", toggle);
    navigator.mediaSession.setActionHandler("nexttrack", () => start(at + 1));
    navigator.mediaSession.setActionHandler("previoustrack", () => start(Math.max(0, at - 1)));
  }
}
