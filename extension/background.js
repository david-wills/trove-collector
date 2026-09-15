// Trove browser watcher — the extension half of the live browser collector.
//
// Every SNAPSHOT_MS (and immediately on tab/window events, debounced) this
// service worker sends one snapshot over a native messaging port to the
// trove-collector binary: is a window of this browser focused, the active tab (url +
// title + favicon + how it was navigated to), every audible tab, and the
// total open-tab count. All span/merge logic lives on the Rust side
// (trove-core TabTracker) — the extension is deliberately a dumb sensor, so a
// missed event or a service worker restart costs at most one snapshot
// interval, and the host's gap detection closes spans cleanly.
//
// MV3 lifetime: messages on a native messaging port reset Chrome's service
// worker idle timer (Chrome 110+), so the 5s snapshot interval keeps this
// worker alive while the port is connected. If Chrome kills the worker
// anyway, the port drops (trove-collector flushes open spans on EOF) and the next
// tab/window/alarm event restarts the worker, which reconnects — the
// 'trove-reconnect' alarm guarantees a retry within 30s even with no user
// activity (e.g. background audio still playing).

// Ad-domain set + inspector match patterns for the page observer (globals;
// tiny, generated — see scripts/gen-ad-domains.mjs).
importScripts('observer/ad-domains.js');

const HOST_NAME = 'com.davidwills.trove';
const SNAPSHOT_MS = 5000;
const EVENT_DEBOUNCE_MS = 200;

let port = null;

// Per-tab navigation context, fed by the webNavigation listeners below and
// read by snapshot() for the active tab. Best-effort and in-memory: an MV3
// worker restart wipes it, so a navigation chain that straddles a restart
// loses its referrer — acceptable, matching the "dumb sensor" contract (the
// Rust side never depends on it). Keyed by tabId.
//   nav[tabId]     = { referrer, transition } for the tab's current URL
//   lastUrl[tabId] = the tab's current committed top-frame URL
const nav = {};
const lastUrl = {};

// transitionTypes where the prior page in the same tab is the referrer.
const REFERRER_TRANSITIONS = new Set(['link', 'form_submit']);

chrome.webNavigation.onCommitted.addListener((d) => {
  if (d.frameId !== 0) return; // top frame only
  const prev = lastUrl[d.tabId];
  nav[d.tabId] = {
    referrer: REFERRER_TRANSITIONS.has(d.transitionType) && prev ? prev : '',
    transition: d.transitionType || '',
  };
  lastUrl[d.tabId] = d.url;
});

// Link opened in a new tab/window: the referrer is the source tab's URL.
chrome.webNavigation.onCreatedNavigationTarget.addListener((d) => {
  nav[d.tabId] = { referrer: lastUrl[d.sourceTabId] || '', transition: 'link' };
  lastUrl[d.tabId] = d.url;
});

chrome.tabs.onRemoved.addListener((tabId) => {
  delete nav[tabId];
  delete lastUrl[tabId];
  dropAdJoins(tabId);
});

function connect() {
  if (port) return true;
  try {
    port = chrome.runtime.connectNative(HOST_NAME);
  } catch (e) {
    port = null;
    return false;
  }
  port.onDisconnect.addListener(() => {
    // Host manifest missing (trove-collector not installed yet) or the host exited.
    // Reading lastError keeps Chrome from logging "Unchecked runtime.lastError".
    void chrome.runtime.lastError;
    port = null;
  });
  return true;
}

function tabInfo(t) {
  return { url: t.url, title: t.title || '', favicon: t.favIconUrl || '' };
}

async function snapshot() {
  const win = await chrome.windows.getLastFocused().catch(() => null);
  const all = await chrome.tabs.query({}).catch(() => []);
  const [active] = await chrome.tabs
    .query({ active: true, lastFocusedWindow: true })
    .catch(() => []);
  let activeInfo = null;
  if (active && active.url) {
    const n = nav[active.id] || {};
    activeInfo = {
      ...tabInfo(active),
      referrer: n.referrer || '',
      transition: n.transition || '',
    };
  }
  return {
    type: 'snapshot',
    focused: !!(win && win.focused),
    tab_count: all.length,
    active: activeInfo,
    audible: all.filter((t) => t.audible && t.url).map(tabInfo),
  };
}

let sendQueued = false;
function send() {
  // Bursts of tab events (a page load fires several onUpdated) collapse
  // into one snapshot.
  if (sendQueued) return;
  sendQueued = true;
  setTimeout(async () => {
    sendQueued = false;
    if (!connect()) return;
    const snap = await snapshot();
    try {
      port.postMessage(snap);
    } catch (e) {
      port = null;
    }
  }, EVENT_DEBOUNCE_MS);
}

// Top-level registration runs again on every worker start, so the steady
// cadence and listeners survive worker restarts.
setInterval(send, SNAPSHOT_MS);
chrome.tabs.onActivated.addListener(send);
chrome.tabs.onRemoved.addListener(send);
chrome.windows.onFocusChanged.addListener(send);
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if ('url' in changeInfo || 'title' in changeInfo || 'audible' in changeInfo) send();
});
chrome.alarms.create('trove-reconnect', { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === 'trove-reconnect') send();
});
send();

// ---------------------------------------------------------------------------
// Page observer — the opt-in content-script arm (docs/page-observer-spec.md).
//
// Everything below is inert until the user enables page observation on the
// options page: with it off there are no host permissions, no registered
// content scripts, and no messages — the extension is byte-for-byte the dumb
// snapshot watcher above.

const OBSERVER_ORIGINS = { origins: ['<all_urls>'] };
const OBSERVER_DEFAULTS = { enabled: false, features: { ads: false } };

async function observerState() {
  const { pageObserver } = await chrome.storage.local.get('pageObserver');
  return {
    ...OBSERVER_DEFAULTS,
    ...pageObserver,
    features: { ...OBSERVER_DEFAULTS.features, ...(pageObserver && pageObserver.features) },
  };
}

// Reconcile desired state (storage flags + actual permission grant) against
// chrome.scripting's registered scripts. Runs on every worker start and on
// every flag/permission change, so a worker restart or browser update can't
// leave scripts half-registered — and a revoked permission (the user can pull
// it in chrome://extensions) tears the scripts down even with flags still on.
async function reconcileObserver() {
  const s = await observerState();
  const granted = await chrome.permissions.contains(OBSERVER_ORIGINS).catch(() => false);

  const want = [];
  if (s.enabled && granted && s.features.ads) {
    want.push(
      {
        id: 'trove-ad-detector',
        matches: ['<all_urls>'],
        js: ['observer/ad-domains.js', 'observer/ad-links.js', 'observer/detector.js'],
        runAt: 'document_idle',
        allFrames: false, // hard constraint: top frame only
        persistAcrossSessions: true,
      },
      {
        id: 'trove-ad-inspector',
        matches: TROVE_AD_MATCH_PATTERNS, // ad domains only — never the page
        js: ['observer/ad-domains.js', 'observer/ad-links.js', 'observer/inspector.js'],
        runAt: 'document_idle',
        allFrames: true,
        persistAcrossSessions: true,
      }
    );
  }

  const have = await chrome.scripting.getRegisteredContentScripts().catch(() => []);
  const haveIds = new Set(have.map((c) => c.id));
  const wantIds = new Set(want.map((c) => c.id));
  const stale = [...haveIds].filter((id) => !wantIds.has(id));
  if (stale.length) {
    await chrome.scripting.unregisterContentScripts({ ids: stale }).catch(() => {});
  }
  const missing = want.filter((c) => !haveIds.has(c.id));
  if (missing.length) {
    try {
      await chrome.scripting.registerContentScripts(missing);
    } catch (e) {
      console.warn('trove: observer script registration failed:', e);
    }
  }
  // Already-registered ids get their definitions refreshed — an extension
  // update can change ad-domains.js match patterns under a stable id.
  const existing = want.filter((c) => haveIds.has(c.id));
  if (existing.length) {
    await chrome.scripting.updateContentScripts(existing).catch(() => {});
  }
}

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === 'local' && changes.pageObserver) reconcileObserver();
});
chrome.permissions.onRemoved.addListener(reconcileObserver);
chrome.permissions.onAdded.addListener(reconcileObserver);
reconcileObserver(); // top level: runs again on every worker start

// Inspector reports, joined to detector records by (tabId, frame URL). The
// join is enrichment only — unjoined records ship without landing_url. Lost
// on worker restart by design (matches the dumb-sensor cost model).
const adJoins = new Map(); // `${tabId}|${frameUrl}` -> {landingUrl, whyUrl}
const AD_JOINS_CAP = 500;

function dropAdJoins(tabId) {
  const prefix = tabId + '|';
  for (const key of adJoins.keys()) {
    if (key.startsWith(prefix)) adJoins.delete(key);
  }
}

// A top-frame navigation invalidates the tab's joins (new document, new ads).
chrome.webNavigation.onCommitted.addListener((d) => {
  if (d.frameId === 0) dropAdJoins(d.tabId);
});

chrome.runtime.onMessage.addListener((msg, sender) => {
  if (!msg || !sender.tab || sender.id !== chrome.runtime.id) return;
  if (msg.kind === 'ad-frame-info') {
    // Prefer the sender's real frame URL over the message field — pages
    // can't forge extension messages, but keep the trust anchor anyway.
    const frameUrl = sender.url || msg.frameUrl;
    if (!frameUrl || typeof msg.landingUrl !== 'string') return;
    adJoins.set(sender.tab.id + '|' + frameUrl, {
      landingUrl: msg.landingUrl,
      whyUrl: typeof msg.whyUrl === 'string' ? msg.whyUrl : '',
    });
    if (adJoins.size > AD_JOINS_CAP) {
      adJoins.delete(adJoins.keys().next().value); // oldest insertion
    }
  } else if (msg.kind === 'ad-events' && Array.isArray(msg.events)) {
    forwardAdEvents(sender.tab.id, msg.events);
  }
});

// Tagged message on the existing native port — the host dispatches on
// `type`. If trove-collector is down the batch is dropped: same idle-and-retry
// posture as snapshots, and ads are observational (no backfill exists).
function forwardAdEvents(tabId, events) {
  if (!connect()) return;
  const enriched = events.map((e) => {
    const join = adJoins.get(tabId + '|' + e.frame_url) || {};
    return {
      ...e,
      landing_url: e.landing_url || join.landingUrl || '',
      why_url: e.why_url || join.whyUrl || '',
    };
  });
  try {
    port.postMessage({ type: 'ads', events: enriched });
  } catch (e) {
    port = null;
  }
}
