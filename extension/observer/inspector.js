// Trove page observer — ad-frame inspector.
//
// Runs only inside frames served from TROVE_AD_INSPECT_DOMAINS (registered
// with allFrames:true but ad-domain match patterns only — injection cost
// lands on actual ad frames, never the page or its other iframes). Its one
// job: find the click-through landing URL, which names the *advertiser*
// rather than the network, plus the AdChoices transparency URL, and report
// them once to the service worker. The worker joins them to the detector's
// record by (tabId, frame URL); the join is enrichment, never a gate — see
// docs/page-observer-spec.md.
//
// Link extraction (troveScanLinks / troveLandingFrom / troveWhyFrom) lives in
// observer/ad-links.js, loaded before this — the detector reads same-origin
// friendly iframes with the very same logic, so the two cannot drift.

(() => {
  'use strict';
  if (window.top === window) return; // ad creatives live in subframes

  let sent = false;
  let whyUrl = ''; // AdChoices transparency link, accumulated as we scan

  // Report once. `landingUrl` may be empty (unattributable click chain) yet
  // `whyUrl` present — still worth sending, the resolver works off it.
  function report(landingUrl) {
    if (sent || (!landingUrl && !whyUrl)) return;
    sent = true;
    try {
      chrome.runtime
        .sendMessage({ kind: 'ad-frame-info', frameUrl: location.href, landingUrl, whyUrl })
        .catch(() => {});
    } catch {
      // Worker gone; the record just ships without enrichment.
    }
  }

  // Returns true once the advertiser landing is found (the valuable signal).
  // The transparency link is captured as a side effect whenever it appears.
  function scan() {
    if (sent) return true;
    const { landing, why } = troveScanLinks(document);
    if (why && !whyUrl) whyUrl = why;
    if (landing) {
      report(landing);
      return true;
    }
    return false;
  }

  if (scan()) return;

  // The creative often renders after document_idle. Watch this (small) frame
  // briefly, then give up — an unjoined record is acceptable by design. On
  // timeout, still report whatever transparency link we found.
  const stop = () => {
    mo.disconnect();
    clearTimeout(timer);
    report('');
  };
  const mo = new MutationObserver(() => {
    if (scan()) stop();
  });
  mo.observe(document.documentElement, { childList: true, subtree: true });
  const timer = setTimeout(stop, 15000);
})();
