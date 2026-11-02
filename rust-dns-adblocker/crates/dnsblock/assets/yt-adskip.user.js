// ==UserScript==
// @name         dnsblock YouTube Ad-Skipper
// @namespace    dnsblock
// @version      0.3.0
// @description  Begleitscript zum Rust-DNS-Adblocker: überspringt Player-Ads,
//               klickt "Skip"-Buttons und blendet Werbefelder im Feed/Overlay
//               aus. Zusätzlich adoptiert SponsorBlock-Strategie: überspringt
//               Content-Segmente (sponsor/selfpromo/intro/outro/preview/filler)
//               per Player-Seek. DNS allein blockt YouTube-Ads nicht – Anzeigen
//               stammen aus derselben Infrastruktur (googlevideo.com) wie die Videos.
// @author       dnsblock
// @match        https://www.youtube.com/*
// @match        https://m.youtube.com/*
// @exclude      https://www.youtube.com/iframe_api
// @grant        none
// @run-at       document-start
// @noframes
// @license      MIT
// ==/UserScript==

/*
 * Strategie (bewusst fertig, datenschutzfreundlich, ohne Netzwerkaufrufe):
 *  1.) CSS ausblenden von Ad-Elementen im Feed und im Player.
 *  2.) MutationObserver: erkennt neue Ad-Slots, sofort behandeln.
 *  3.) Skip-Buttons frühzeitig klicken (`ytp-skip-ad...`), bevor der Skin
 *      noch ausgebaut ist.
 *  4.) Feed-Ads direkt entfernen (DOM), um Perf zu sparen.
 *  5.) SponsorBlock-Style: Content-Segmente (sponsor/intro/outro/...) via
 *      Server-Query + Player-Seek überspringen.
 *  6.) Keine Google-IPs/Domains anfassen – Video-Playback bleibt unbeschädigt.
 */
(function () {
  "use strict";

  // ---------- 1) CSS ausblenden ----------
  const CSS = `
    /* Player-Ads */
    .ytp-player-content.ytp-iv-player-content,
    .iv-promo,
    .ytp-ad-message-container,
    .ytp-preview-ad__text,
    ytd-action-companion-ad-renderer,
    ytd-companion-slot-renderer,
    ytd-player-legacy-desktop-watch-ads-renderer,
    .video-ads .ytp-ad-module,
    .ytp-ad-player-overlay { display: none !important; }

    /* Masthead-/Feed-/Such-Ads */
    #masthead-ad,
    ytd-rich-item-renderer[ytd-ad-slot-renderer],
    ytd-ad-slot-renderer,
    ytd-in-feed-ad-layout-renderer,
    ytd-display-ad-renderer,
    ytd-promoted-sparkles-web-renderer,
    ytd-promoted-video-renderer,
    ytd-rich-aditem-renderer,
    ytd-carousel-item-renderer .ytd-promoted-sparkles-text-search-renderer,
    yt-mealbar-promo-renderer { display: none !important; }
  `;
  function addStyle() {
    const s = document.createElement("style");
    s.textContent = CSS;
    (document.head || document.documentElement).appendChild(s);
  }

  // ---------- 2) Skip / Close / Entfernen ----------
  const CLEAN_SELECTORS = [
    "ytd-ad-slot-renderer",
    "ytd-in-feed-ad-layout-renderer",
    "ytd-display-ad-renderer",
    "ytd-promoted-sparkles-web-renderer",
    "ytd-promoted-video-renderer",
    "ytd-rich-aditem-renderer",
    "ytd-companion-slot-renderer",
    "ytd-action-companion-ad-renderer",
    "ytd-player-legacy-desktop-watch-ads-renderer",
    "#masthead-ad",
    ".iv-promo",
    ".ytp-ad-message-container",
  ].join(",");

  function clickSkipButtons() {
    const skipSel = ".ytp-skip-ad-button, .ytp-skip-ad-button-modern, .ytp-ad-skip-button, .ytp-ad-skip-button-modern";
    document.querySelectorAll(skipSel).forEach(btn => {
      try { btn.click(); } catch { /*ignore*/ }
    });
    document.querySelectorAll(
      ".ytp-ad-overlay-close-button, .ytp-ad-overlay-close-container"
    ).forEach(b => { try { b.click(); } catch { /*ignore*/ } });
  }

  function cleanFeed() {
    document.querySelectorAll(CLEAN_SELECTORS).forEach(el => {
      try { el.remove(); } catch { /*ignore*/ }
    });
  }

  // ---------- 3) SponsorBlock-Style: Content-Segmente überspringen ----------
  const SKIP_CATEGORIES = ["sponsor", "selfpromo", "intro", "outro", "preview", "filler"];
  const SB_SERVER = "https://sponsor.ajay.app";

  function sbHash(s) {
    let h = 0x811c9dc5;
    for (let i = 0; i < s.length; i++) {
      h ^= s.charCodeAt(i);
      h = Math.imul(h, 0x01000193) >>> 0;
    }
    return h.toString(16).padStart(8, "0").slice(0, 5);
  }

  function getVideoId() {
    const watch = location.pathname.match(/\/watch\?v=([\w-]{11})/);
    if (watch) return watch[1];
    const sh = location.pathname.match(/\/(?:shorts|embed|live)\/([\w-]{11})/);
    if (sh) return sh[1];
    const v = new URLSearchParams(location.search).get("v");
    if (v && /^[\w-]{11}$/.test(v)) return v;
    return null;
  }

  const segState = { videoId: null, segments: [] };
  const skippedSegs = new Set();

  async function refreshSegments(videoId) {
    try {
      const r = await fetch(SB_SERVER + "/api/skipSegments/" + sbHash(videoId));
      if (!r.ok) return;
      const data = await r.json();
      const entries = Array.isArray(data) ? data : [];
      const segs = [];
      for (const e of entries) {
        if (e && e.videoID && e.videoID === videoId && Array.isArray(e.segments)) {
          for (const s of e.segments) {
            if (s && Array.isArray(s.segment) && s.actionType === "skip") {
              segs.push({ category: s.category, start: s.segment[0], end: s.segment[1], UUID: s.UUID });
            }
          }
        }
      }
      segState.videoId = videoId;
      segState.segments = segs;
    } catch (err) { /* Netzwerkfehler etc. – kein Crash */ }
  }

  function skipSegment() {
    const v = document.querySelector("video");
    if (!v) return;
    const t = v.currentTime || 0;
    const segs = segState.videoId ? segState.segments : [];
    for (const s of segs) {
      if (!s || !s.UUID) continue;
      if (SKIP_CATEGORIES.indexOf(s.category) === -1) continue;
      if (skippedSegs.has(s.UUID)) continue;
      if (t >= s.start && t < s.end) {
        skippedSegs.add(s.UUID);
        try { v.currentTime = s.end; } catch (err) { /* ignore */ }
        return true;
      }
    }
    return false;
  }

  function sweep() {
    clickSkipButtons();
    cleanFeed();
    const vid = getVideoId();
    if (vid && vid !== segState.videoId) {
      segState.videoId = vid;
      segState.segments = [];
      refreshSegments(vid);
    }
    skipSegment();
  }

  // ---------- 4) Boot ----------
  function boot() {
    addStyle();
    sweep();
    const observer = new MutationObserver(sweep);
    observer.observe(document.documentElement, { childList: true, subtree: true });
    setInterval(sweep, 500);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot, { once: true });
  } else {
    boot();
  }
})();
