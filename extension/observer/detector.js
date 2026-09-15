// Trove page observer — ad detector (top frame only).
//
// Finds display-ad iframes in the page, measures how long each was actually
// on screen, and ships one record per ad to the service worker when the ad
// leaves the DOM or the page unloads. Registered dynamically (never in the
// manifest) only while the "ads" feature is on — see background.js
// reconcileObserver(). Loaded after observer/ad-domains.js, which provides
// TROVE_AD_DOMAINS / TROVE_AD_SLOT_PREFIXES / troveAdDomainOf.
//
// Performance contract (docs/page-observer-spec.md): one scan at injection;
// after that, a childList-only MutationObserver debounced 500ms is the only
// page-wide hook. Viewability is IntersectionObserver-only — no polling, no
// scroll listeners, and no layout reads (w/h come from the observer's own
// entries). Sends are batched; nothing here runs per-frame or per-scroll.

(() => {
  'use strict';
  if (window.top !== window) return; // belt and braces; registered allFrames:false

  const SCAN_DEBOUNCE_MS = 500;
  const VIEWABLE_MS = 1000; // MRC: ≥50% visible for ≥1 continuous second
  const DIRTY_FLUSH_MS = 30000;
  const MAX_PENDING = 100; // drop oldest beyond — observational data
  const MAX_QUEUED_NODES = 400; // added-subtree queue bound (overflow = full rescan)
  // Creatives render after the iframe appears, so the click-through link read
  // (same-origin friendly iframes only) retries a few times before giving up.
  const LINK_READ_MS = 1500;
  const LINK_READ_TRIES = 3;

  /** @type {Map<Element, object>} live records, keyed by iframe element */
  const records = new Map();
  /** closed records waiting to ship, oldest first */
  const pending = [];

  function hostOf(url) {
    try {
      return new URL(url, location.href).hostname;
    } catch {
      return '';
    }
  }

  // Detection signal 2: ad-slot naming on the iframe (or its container) for
  // ads rendered into srcdoc/about:blank frames. Prefix checks only.
  function slotOf(el) {
    for (const prefix of Object.keys(TROVE_AD_SLOT_PREFIXES)) {
      if (
        (el.id && el.id.startsWith(prefix)) ||
        (el.name && el.name.startsWith(prefix))
      ) {
        return prefix;
      }
    }
    if (el.closest('[id^="div-gpt-ad"]')) return 'div-gpt-ad';
    return '';
  }

  function maybeOpen(el) {
    if (records.has(el)) return;
    const src = el.src || '';
    const isHttp = src.startsWith('https://') || src.startsWith('http://');
    let frameUrl = '';
    let slot = '';
    if (isHttp && troveAdDomainOf(hostOf(src))) {
      frameUrl = src; // signal 1: served from a known ad domain
      slot = slotOf(el); // network fallback if the URL ever fails to parse
    } else {
      slot = slotOf(el);
      if (!slot) return;
      if (isHttp) frameUrl = src; // slot-named frame on a non-ad domain (rare)
    }
    records.set(el, {
      frameUrl,
      slot,
      tsMs: Date.now(),
      viewedMs: 0,
      visibleSince: 0, // performance.now() while ≥50% visible & tab visible
      lastRatio: 0,
      viewable: false,
      w: 0,
      h: 0, // from IntersectionObserver entries — never read from layout
      landing: '', // advertiser click-through, read from a friendly iframe
      why: '', // AdChoices transparency URL, same source
    });
    io.observe(el);
    // Cross-origin creatives (the inspector's job) read null here and are
    // skipped; same-origin friendly iframes (GPT's google_ads_iframe, often
    // slot-detected with no frame URL) expose their click-through href to us
    // directly — the only way to attribute those, and no network call.
    setTimeout(() => tryReadLink(el, LINK_READ_TRIES), LINK_READ_MS);
  }

  // Read the creative's advertiser landing (and AdChoices) link out of a
  // same-origin friendly iframe, retrying while it renders. Best-effort: a
  // cross-origin frame, a sandboxed one, or a slow creative just leaves the
  // record unattributed.
  function tryReadLink(el, triesLeft) {
    const r = records.get(el);
    if (!r || r.landing) return; // closed, or already attributed
    let doc = null;
    try {
      doc = el.contentDocument; // null for cross-origin — no throw, but guard anyway
    } catch {
      return;
    }
    if (doc) {
      const { landing, why } = troveScanLinks(doc);
      if (why && !r.why) r.why = why;
      if (landing) {
        r.landing = landing;
        return;
      }
    }
    if (triesLeft > 1 && el.isConnected) {
      setTimeout(() => tryReadLink(el, triesLeft - 1), LINK_READ_MS);
    }
  }

  // Start/stop the viewed-time clock. `span >= VIEWABLE_MS` on stop is the
  // continuity check — no timer needed, pauses and closes are the only
  // moments continuity can end.
  function setInView(r, inView, now) {
    if (inView && !r.visibleSince) {
      r.visibleSince = now;
    } else if (!inView && r.visibleSince) {
      const span = now - r.visibleSince;
      r.viewedMs += span;
      if (span >= VIEWABLE_MS) r.viewable = true;
      r.visibleSince = 0;
    }
  }

  const io = new IntersectionObserver(
    (entries) => {
      const now = performance.now();
      const visible = document.visibilityState === 'visible';
      for (const e of entries) {
        const r = records.get(e.target);
        if (!r) continue;
        r.lastRatio = e.intersectionRatio;
        const rect = e.boundingClientRect;
        if (rect.width > r.w) r.w = Math.round(rect.width);
        if (rect.height > r.h) r.h = Math.round(rect.height);
        setInView(r, visible && e.intersectionRatio >= 0.5, now);
      }
    },
    { threshold: [0, 0.5] }
  );

  // IntersectionObserver stops firing in background tabs, so the visibility
  // gate pauses/resumes the clock itself.
  document.addEventListener('visibilitychange', () => {
    const now = performance.now();
    const visible = document.visibilityState === 'visible';
    for (const r of records.values()) {
      setInView(r, visible && r.lastRatio >= 0.5, now);
    }
    if (!visible) flush(); // ship whatever is closed when backgrounded
  });

  function close(el) {
    const r = records.get(el);
    if (!r) return;
    records.delete(el); // no retained DOM references after close
    io.unobserve(el);
    setInView(r, false, performance.now());
    pending.push({
      ts_ms: r.tsMs,
      end_ms: Date.now(),
      page_url: location.href,
      frame_url: r.frameUrl,
      slot: r.slot,
      landing_url: r.landing,
      why_url: r.why,
      viewed_secs: Math.round(r.viewedMs / 100) / 10,
      viewable: r.viewable,
      w: r.w,
      h: r.h,
    });
    if (pending.length > MAX_PENDING) pending.splice(0, pending.length - MAX_PENDING);
    scheduleFlush();
  }

  function scanForIframes(root) {
    if (root.tagName === 'IFRAME') maybeOpen(root);
    if (root.querySelectorAll) {
      for (const el of root.querySelectorAll('iframe')) maybeOpen(el);
    }
  }

  // childList-only and debounced — the one page-wide observer allowed, and
  // it only walks added subtrees (or, past the queue bound, runs one cheap
  // document-wide iframe query) plus a disconnect check on tracked ads.
  let queued = []; // added element nodes since the last tick; null = overflow
  let scanTimer = 0;
  const mo = new MutationObserver((muts) => {
    if (queued) {
      for (const m of muts) {
        for (const n of m.addedNodes) {
          if (n.nodeType !== Node.ELEMENT_NODE) continue;
          if (queued.length >= MAX_QUEUED_NODES) {
            queued = null;
            break;
          }
          queued.push(n);
        }
        if (!queued) break;
      }
    }
    if (scanTimer) return;
    scanTimer = setTimeout(() => {
      scanTimer = 0;
      for (const el of [...records.keys()]) {
        if (!el.isConnected) close(el); // removed or replaced (ad refresh)
      }
      const batch = queued;
      queued = [];
      if (batch) {
        for (const n of batch) if (n.isConnected) scanForIframes(n);
      } else {
        for (const el of document.querySelectorAll('iframe')) maybeOpen(el);
      }
    }, SCAN_DEBOUNCE_MS);
  });

  // --- batching ------------------------------------------------------------

  let dirtyTimer = 0;
  function scheduleFlush() {
    if (!dirtyTimer && pending.length) {
      dirtyTimer = setTimeout(flush, DIRTY_FLUSH_MS);
    }
  }

  function flush() {
    if (dirtyTimer) {
      clearTimeout(dirtyTimer);
      dirtyTimer = 0;
    }
    if (!pending.length) return;
    const events = pending.splice(0);
    const requeue = () => {
      // Worker was asleep/dead: put the batch back (still oldest-first) and
      // let the next trigger retry. The cap above bounds the buffer.
      pending.unshift(...events);
      if (pending.length > MAX_PENDING) pending.splice(0, pending.length - MAX_PENDING);
    };
    try {
      chrome.runtime.sendMessage({ kind: 'ad-events', events }).catch(requeue);
    } catch {
      requeue();
    }
  }

  addEventListener('pagehide', () => {
    for (const el of [...records.keys()]) close(el);
    flush();
  });

  scanForIframes(document.documentElement);
  mo.observe(document.body || document.documentElement, { childList: true, subtree: true });
})();
