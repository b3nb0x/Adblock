// ==UserScript==
// @name         dnsblock YouTube Ad-Skipper
// @namespace    dnsblock
// @version      0.2.0
// @description  Begleitscript zum Rust-DNS-Adblocker: überspringt Player-Ads,
//               klickt "Skip"-Buttons und blendet Werbefelder im Feed/Overlay
//               aus. DNS allein blockt YouTube-Ads nicht – Anzeigen stammen
//               aus derselben Infrastruktur (googlevideo.com) wie die Videos.
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
 *  5.) Keine Google-IPs/Domains anfassen – Video-Playback bleibt unbeschädigt.
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
    // Ads aus unblockierbaren Ort (Feed / Masthead / Search)
    "ytd-ad-slot-renderer",
    "ytd-in-feed-ad-layout-renderer",
    "ytd-display-ad-renderer",
    "ytd-promoted-sparkles-web-renderer",
    "ytd-rich-aditem-renderer",
    "ytd-companion-slot-renderer",
    "ytd-action-companion-ad-renderer",
    "ytd-player-legacy-desktop-watch-ads-renderer",
    "#masthead-ad",
    ".iv-promo",
    ".ytp-ad-message-container",
  ].join(",");

  function clickSkipButtons() {
    // Desktop + mobile Varianten
    const skipSel = ".ytp-skip-ad-button, .ytp-skip-ad-button-modern, .ytp-ad-skip-button, .ytp-ad-skip-button-modern";
    document.querySelectorAll(skipSel).forEach(btn => {
      try { btn.click(); } catch { /*ignore*/ }
    });
    // Overlay-Banner schließen (falls "X" erscheint)
    document.querySelectorAll(
      ".ytp-ad-overlay-close-button, .ytp-ad-overlay-close-container"
    ).forEach(b => { try { b.click(); } catch { /*ignore*/ } });
  }

  function cleanFeed() {
    document.querySelectorAll(CLEAN_SELECTORS).forEach(el => {
      try { el.remove(); } catch { /*ignore*/ }
    });
  }

  function sweep() {
    clickSkipButtons();
    cleanFeed();
  }

  // ---------- 3) Boot ----------
  function boot() {
    addStyle();
    sweep();
    const observer = new MutationObserver(sweep);
    observer.observe(document.documentElement, { childList: true, subtree: true });
    // Fallback-Pulses (MutationObserver reicht allein nicht immer)
    setInterval(sweep, 500);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot, { once: true });
  } else {
    boot();
  }
})();
