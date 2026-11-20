// `true` when the page is scrolled to its end: inside the window on wide
// screens (panel.css), the document on phones.
window.glyphAtEnd = () => {
  const c = document.querySelector(".content");
  if (c && c.scrollHeight > c.clientHeight + 1) return c.scrollTop + c.clientHeight >= c.scrollHeight - 4;
  return innerHeight + scrollY >= document.documentElement.scrollHeight - 4;
};

// Soft edges where a list or page scrolls on (ui.svelte.ts fadeEdges): the
// top fades in once scrolled, the bottom while more is below. Sizes as in
// the app: the page 22 px (SettingsModal), the nav 18 px (Nav.svelte).
// `data-fade-top="0"` turns the top edge off (a sticky bar is the edge);
// axis "x" fades the left and right edges of a row that scrolls sideways.
window.glyphFadeEdges = (el, size, minOverflow = 32, axis = "y") => {
  let last = "", raf = 0;
  const ramp = (k, a, at) => `rgba(0,0,0,${(1 - k * a).toFixed(3)}) ${at}`;
  const update = () => {
    raf = 0;
    const y = axis === "y", total = y ? el.clientHeight : el.clientWidth, max = (y ? el.scrollHeight : el.scrollWidth) - total, pos = y ? el.scrollTop : el.scrollLeft;
    let sk = 0, ek = 0;
    if (total > 0 && max >= minOverflow) {
      const r = Math.min(40, Math.max(8, max * 0.5));
      sk = Math.min(1, Math.max(0, (pos - 2) / r));
      ek = Math.min(1, Math.max(0, (max - pos - 2) / r));
    }
    let s = el.dataset.fadeTop !== undefined ? +el.dataset.fadeTop : size, e = size;
    let mask = "";
    if (sk > 0.001 || ek > 0.001) {
      const cap = total * 0.85;
      if (s + e > cap) { const k = cap / (s + e); s *= k; e *= k; }
      const stops = [];
      if (sk > 0.001 && s > 1) {
        const pad = Math.min(2, s * 0.1), span = s - pad;
        stops.push(ramp(sk, 1, "0px"), ramp(sk, 1, pad.toFixed(1) + "px"), ramp(sk, 0.844, (pad + span * 0.25).toFixed(1) + "px"),
          ramp(sk, 0.5, (pad + span * 0.5).toFixed(1) + "px"), ramp(sk, 0.156, (pad + span * 0.75).toFixed(1) + "px"), `#000 ${s.toFixed(1)}px`);
      } else stops.push("#000 0px");
      if (ek > 0.001 && e > 1) {
        const pad = Math.min(2, e * 0.1), span = e - pad, at = d => `calc(100% - ${d.toFixed(1)}px)`;
        stops.push(`#000 ${at(e)}`, ramp(ek, 0.156, at(pad + span * 0.75)), ramp(ek, 0.5, at(pad + span * 0.5)),
          ramp(ek, 0.844, at(pad + span * 0.25)), ramp(ek, 1, at(pad)), ramp(ek, 1, "100%"));
      } else stops.push("#000 100%");
      mask = `linear-gradient(${y ? "to bottom" : "to right"}, ${stops.join(", ")})`;
    }
    if (mask !== last) { last = mask; el.style.maskImage = mask; el.style.webkitMaskImage = mask; }
  };
  const schedule = () => { if (!raf) raf = requestAnimationFrame(update); };
  el.addEventListener("scroll", schedule, { passive: true });
  const ro = new ResizeObserver(schedule);
  ro.observe(el);
  for (const c of el.children) ro.observe(c);
  new MutationObserver(schedule).observe(el, { childList: true, subtree: true, attributes: true, attributeFilter: ["hidden"] });
  schedule();
};
document.querySelectorAll(".content").forEach(el => glyphFadeEdges(el, 22));
document.querySelectorAll(".nav .list, .nav, .otp").forEach(el => glyphFadeEdges(el, 18, 12));

// Smooth mouse wheel: each notch glides to its target with an ease-out
// instead of jumping. Touchpads (small pixel steps) and touch keep the
// browser's own scrolling, which is already smooth. The innermost list
// that can still move takes the wheel, like native scroll chaining.
window.glyphSmoothWheel = el => {
  let target = el.scrollTop, pos = el.scrollTop, raf = 0, last = 0;
  const max = () => el.scrollHeight - el.clientHeight;
  const step = t => {
    const dt = Math.min(64, t - (last || t));
    last = t;
    pos += (target - pos) * (1 - Math.exp(-dt / 70));
    if (Math.abs(target - pos) < 0.5) pos = target;
    el.scrollTo({ top: pos, behavior: "instant" });
    raf = pos === target ? 0 : requestAnimationFrame(step);
    if (!raf) last = 0;
  };
  el.addEventListener("wheel", e => {
    if (e.defaultPrevented || e.ctrlKey || e.shiftKey || Math.abs(e.deltaX) > Math.abs(e.deltaY)) return;
    const px = e.deltaMode === 1 ? e.deltaY * 40 : e.deltaMode === 2 ? e.deltaY * el.clientHeight : e.deltaY;
    const touchpad = e.deltaMode === 0 && Math.abs(e.deltaY) < 50;
    if (touchpad || max() <= 0) return;
    if (!raf) { pos = el.scrollTop; target = pos; }
    const next = Math.max(0, Math.min(max(), target + px));
    if (next === target && (target <= 0 || target >= max())) return; // at the end: let the outer list move
    e.preventDefault();
    target = next;
    if (!raf) raf = requestAnimationFrame(step);
  }, { passive: false });
  // The scrollbar, keys or a jump take over: start from where they left it.
  for (const ev of ["pointerdown", "keydown"]) el.addEventListener(ev, () => { cancelAnimationFrame(raf); raf = 0; last = 0; });
};
if (matchMedia("(pointer: fine) and (prefers-reduced-motion: no-preference)").matches) document.querySelectorAll(".content, .nav .list, .otp").forEach(glyphSmoothWheel);

// Phone menu of the panel pages (panel.css): the menu button in the titlebar
// slides the nav in from the side as a full screen; the back arrow, Esc, a
// link in it or a wider window close it again.
(() => {
  const nav = document.getElementById("menu");
  const btn = document.querySelector(".menubtn");
  if (!nav || !btn) return;
  const set = open => {
    nav.classList.toggle("open", open);
    document.body.classList.toggle("menu-open", open);
    btn.setAttribute("aria-expanded", String(open));
    if (open) {
      const cur = nav.querySelector(".list [aria-current], .list .on, .list .sel");
      if (cur) cur.scrollIntoView({ block: "center" });
      nav.querySelector(".mhead .bk")?.focus({ preventScroll: true });
    } else if (nav.contains(document.activeElement)) {
      btn.focus({ preventScroll: true });
    }
  };
  btn.addEventListener("click", () => set(true));
  nav.querySelector(".mhead .bk")?.addEventListener("click", () => set(false));
  nav.addEventListener("click", e => { if (e.target.closest("a[href], [data-close]")) set(false); });
  addEventListener("keydown", e => { if (e.key === "Escape" && nav.classList.contains("open")) set(false); });
  matchMedia("(min-width: 861px)").addEventListener("change", e => { if (e.matches) set(false); });
  window.glyphMenu = set;
})();
