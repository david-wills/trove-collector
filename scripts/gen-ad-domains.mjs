#!/usr/bin/env node
// Regenerates extension/observer/ad-domains.js — the compiled-in ad-domain
// set the page observer's detector and inspector share (see
// docs/page-observer-spec.md §Filter list).
//
// Run manually, commit the output. No runtime fetch ever happens in the
// extension; this script is the only thing that touches the network.
//
//   node scripts/gen-ad-domains.mjs            # fetches EasyList
//   node scripts/gen-ad-domains.mjs easylist.txt  # or use a local copy
//
// Design: EasyList's ad-server sections list ~20k domains, but almost all of
// it is pop-under/redirect junk that never serves a display iframe on a
// mainstream page — and a 20k-entry set would blow the detector's per-page
// parse budget. So the curated SEED below (the major ad servers, exchanges,
// and SSPs whose iframes one actually encounters) is the source of truth for
// *which organizations* count as ad networks, and EasyList is the source of
// truth for *whether each domain really is an ad server*: every seed is
// checked against the adservers sections and the script fails loudly on
// drift (a seed EasyList no longer blocks is flagged for review, not
// silently kept). EasyList also contributes sibling domains it blocks under
// the same organization (e.g. criteo.net next to criteo.com).

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const EASYLIST_URL = 'https://easylist.to/easylist/easylist.txt';
const OUT = join(dirname(fileURLToPath(import.meta.url)), '../extension/observer/ad-domains.js');

// eTLD+1s of the major display-ad networks: ad servers, exchanges, SSPs,
// retargeters, native-ad widgets, and the measurement domains that ride
// along in ad iframes. Grouped loosely; order is cosmetic (output is sorted).
const SEED = [
  // Google
  'doubleclick.net', 'googlesyndication.com', 'googleadservices.com',
  'googletagservices.com', 'admob.com', '2mdn.net', 'adsense.com',
  // Amazon
  'amazon-adsystem.com',
  // Microsoft/Xandr/AppNexus
  'adnxs.com', 'adnxs-simple.com', 'msads.net',
  'adsymptotic.com',
  // Criteo
  'criteo.com', 'criteo.net', 'emailretargeting.com',
  // Native / content-recommendation
  'taboola.com', 'taboolasyndication.com', 'outbrain.com', 'outbrainimg.com',
  'revcontent.com', 'mgid.com', 'zergnet.com', 'dianomi.com', 'nativo.com',
  'ntv.io', 'sharethrough.com', 'srtb.net', 'plista.com', 'spoutable.com',
  // Major exchanges / SSPs
  'rubiconproject.com', 'magnite.com', 'pubmatic.com', 'openx.net',
  'openx.com', 'casalemedia.com', 'indexww.com', 'indexexchange.com',
  'smartadserver.com', 'sascdn.com', 'adform.net', 'adformdsp.net',
  'improvedigital.com', '360yield.com', 'sovrn.com', 'lijit.com',
  'gumgum.com', 'yieldmo.com', 'triplelift.com', '3lift.com',
  'sonobi.com', 'undertone.com', 'conversantmedia.com', 'dotomi.com',
  'emxdgt.com', 'kargo.com', 'sharethis.com', '33across.com', 'tynt.com',
  'unrulymedia.com', 'unruly.co', 'teads.tv', 'teads.com', 'stickyadstv.com',
  'freewheel.tv', 'fwmrm.net', 'spotx.tv', 'spotxchange.com',
  'springserve.com', 'telaria.com', 'tremorhub.com', 'yieldlab.net',
  'adition.com', 'smartclip.net', 'adscale.de', 'ad-srv.net',
  'richaudience.com', 'seedtag.com', 'ogury.com', 'adyoulike.com',
  'smilewanted.com', 'sublime.xyz', 'mediasquare.fr', 'adagio.io',
  // DSPs / trade desks / retargeters
  'adsrvr.org', 'mediamath.com', 'mathtag.com', 'turn.com', 'amobee.com',
  'tidaltv.com', 'simpli.fi', 'stackadapt.com', 'basis.net', 'sitescout.com',
  'centro.net', 'zemanta.com', 'quantserve.com', 'quantcount.com',
  'rfihub.com', 'rfihub.net', 'steelhousemedia.com', 'adroll.com',
  'perfectaudience.com', 'chango.com', 'retargetly.com',
  // Media / portal networks
  'media.net', 'adtechus.com', 'advertising.com',
  'adtech.de', 'yieldmanager.com', 'flashtalking.com',
  'sizmek.com', 'serving-sys.com', 'innovid.com', 'celtra.com',
  'undertone.com', 'exponential.com', 'tribalfusion.com', 'zedo.com',
  'vibrantmedia.com', 'intellitxt.com', 'kontera.com', 'infolinks.com',
  // Header bidding / wrappers / id
  'rlcdn.com', 'id5-sync.com', 'crwdcntrl.net',
  'bluekai.com', 'demdex.net', 'everesttech.net', 'agkn.com', 'exelator.com',
  'eyeota.net', 'tapad.com', 'liadm.com', 'pippio.com',
  // Verification / viewability (ride along inside ad iframes)
  'moatads.com', 'doubleverify.com', 'adsafeprotected.com', 'iasds01.com',
  'adlightning.com', 'confiant-integrations.net',
  // Pop/performance networks big enough to meet in the wild
  'propellerads.com', 'propellerclick.com', 'adsterra.com', 'adcash.com',
  'popads.net', 'popcash.net', 'exoclick.com', 'exosrv.com',
  'juicyads.com', 'trafficjunky.com', 'trafficfactory.biz', 'adtng.com',
  'adfox.ru',
  'bidvertiser.com', 'revenuehits.com', 'adblade.com', 'adstir.com',
  'adskeeper.com', 'mgid.com',
  // Mobile-web mediation that also serves desktop web
  'inmobi.com', 'smaato.net', 'smaato.com', 'pubnative.net', 'applovin.com',
  'unity3dusercontent.com',
  'vungle.com', 'ironsrc.com', 'supersonicads.com', 'chartboost.com',
  // Misc long-standing ad servers
  'adriver.ru', 'admixer.net', 'admedia.com', 'adkernel.com', 'adlooxtracking.com',
  'bidswitch.net', 'bttrack.com', 'connatix.com', 'contextweb.com',
  'districtm.io', 'gammaplatform.com', 'imonomy.com',
  'loopme.com', 'loopme.me', 'lkqd.net', 'minutemedia-prebid.com',
  'onetag-sys.com', 'powerlinks.com', 'pro-market.net', 'pulsepoint.com',
  'rhythmone.com', '1rx.io', 'sekindo.com', 'somoaudience.com',
  'vidazoo.com', 'vidoomy.com', 'yieldlove.com',
  'yldbt.com', 'aniview.com', 'avantisvideo.com', 'brightcom.com',
  'eskimi.com', 'insurads.com', 'mediafuse.com', 'primis.tech',
  'undertone.com',
].filter(Boolean);

// Slot-name prefixes for ads rendered into srcdoc/about:blank iframes whose
// src carries no domain (detection signal 2 in the spec). Checked with
// String.startsWith against the iframe's id/name and its container's id —
// never regexes. Keys map to the network recorded when frame_url is empty.
const SLOT_PREFIXES = {
  google_ads_iframe: 'google', // GPT render targets
  aswift_: 'google', //           AdSense
  'div-gpt-ad': 'google', //      GPT containers (iframe's parent div)
};

// The subset the *inspector* injects into (allFrames, ad domains only):
// networks that actually serve the creative document and whose click-through
// markup the extractor table understands. Deliberately much smaller than the
// detection set — every entry multiplies injections.
const INSPECT = [
  'doubleclick.net', 'googlesyndication.com', 'googleadservices.com',
  '2mdn.net', 'adnxs.com', 'criteo.com', 'criteo.net',
  'amazon-adsystem.com', 'taboola.com', 'outbrain.com',
  'rubiconproject.com', 'pubmatic.com', 'openx.net', 'casalemedia.com',
  'smartadserver.com', 'adform.net', 'media.net', 'mgid.com',
  'revcontent.com', 'teads.tv', 'flashtalking.com',
];

// Query params that carry the landing URL in click-through hrefs, tried in
// order (adurl is Google's; the rest cover the other inspected networks).
// `click`/`clk` carry the final advertiser URL through second-hop redirectors
// (e.g. Google adclick → dts.innovid.com/clktru?...&click=<advertiser>); the
// unwrap recurses, so a chain like that resolves to the real advertiser.
const CLICK_PARAMS = ['adurl', 'dest_url', 'destination', 'clickurl', 'click', 'clk', 'curl', 'redirect', 'redir', 'r', 'u', 'url'];

// --- EasyList verification -------------------------------------------------

async function easylist() {
  const arg = process.argv[2];
  if (arg) return readFileSync(arg, 'utf8');
  const res = await fetch(EASYLIST_URL);
  if (!res.ok) throw new Error(`fetching EasyList: HTTP ${res.status}`);
  return res.text();
}

// All registrable-ish domains EasyList's adserver/third-party sections block
// (rule forms ||domain^..., with/without options/paths — presence is what matters).
function adserverDomains(text) {
  const lines = text.split('\n');
  const out = new Set();
  let inAdservers = false;
  for (const line of lines) {
    const section = line.match(/^! \*\*\* easylist:(\S+) \*\*\*/);
    if (section) inAdservers = /adservers|thirdparty/.test(section[1]);
    if (!inAdservers || !line.startsWith('||')) continue;
    const m = line.match(/^\|\|([a-z0-9.-]+\.[a-z]{2,})[\^/$]/);
    if (m) out.add(m[1]);
  }
  return out;
}

// Two-part public suffixes common among ad domains, for eTLD+1 reduction.
const TWO_PART = new Set(['co.uk', 'com.au', 'co.jp', 'co.kr', 'com.br', 'co.in', 'com.tr', 'net.au']);

function etld1(host) {
  const parts = host.split('.');
  if (parts.length <= 2) return host;
  const two = parts.slice(-2).join('.');
  return TWO_PART.has(two) ? parts.slice(-3).join('.') : two;
}

const list = await easylist();
const blocked = adserverDomains(list);
const blockedEtld1 = new Set([...blocked].map(etld1));

const seeds = [...new Set(SEED)].sort();
const verified = [];
const unverified = [];
for (const d of seeds) (blockedEtld1.has(d) ? verified : unverified).push(d);

if (unverified.length) {
  console.error(
    `warning: ${unverified.length} seed(s) not in EasyList's adserver/third-party ` +
      `sections — EXCLUDED from output (review for retirement/renames):\n  ${unverified.join('\n  ')}`
  );
}

// Siblings: exact-domain EasyList entries that share a verified seed's
// organization label (e.g. seed criteo.com pulls criteo.net even if the seed
// list forgot it). Label match only — never pulls unrelated junk in bulk.
const labels = new Set(verified.map((d) => d.split('.')[0]));
const siblings = [...blockedEtld1].filter(
  (d) => labels.has(d.split('.')[0]) && !verified.includes(d) && !unverified.includes(d)
);

// Unverified seeds stay out: EasyList is the gate, the warning above is the
// review queue. This is what naturally retires dead networks (Sizmek,
// MediaMath, …) and keeps pure tracker domains from masquerading as ads.
const domains = [...new Set([...verified, ...siblings])].sort();
const inspect = [...new Set(INSPECT)].sort();
for (const d of inspect) {
  if (!domains.includes(d)) throw new Error(`inspect domain ${d} missing from detection set`);
}

const stamp = new Date().toISOString().slice(0, 10);
const body = `// GENERATED by scripts/gen-ad-domains.mjs — do not edit by hand.
// ${domains.length} ad-network eTLD+1s, verified against EasyList ${stamp}.
// Plain script (no modules): loaded before detector.js/inspector.js in
// content scripts, importScripts'd by the service worker, <script>'d by the
// options page. Defines globals only.

// Detection set: an iframe whose src host has any suffix in here is an ad.
const TROVE_AD_DOMAINS = new Set(${JSON.stringify(domains, null, 2)});

// Slot-name prefixes (srcdoc/about:blank ads with no src domain) → network
// fallback. Checked with startsWith on iframe id/name and container id.
const TROVE_AD_SLOT_PREFIXES = ${JSON.stringify(SLOT_PREFIXES, null, 2)};

// Networks the inspector injects into (subset: creative-serving domains the
// click-through extractor understands). Kept here so detector and inspector
// derive from one module and cannot drift.
const TROVE_AD_INSPECT_DOMAINS = ${JSON.stringify(inspect, null, 2)};

// chrome.scripting match patterns for the inspector registration.
const TROVE_AD_MATCH_PATTERNS = TROVE_AD_INSPECT_DOMAINS.flatMap((d) => [
  \`https://\${d}/*\`,
  \`https://*.\${d}/*\`,
]);

// Click-through query params that carry the landing URL, tried in order.
const TROVE_AD_CLICK_PARAMS = ${JSON.stringify(CLICK_PARAMS)};

// The set entry (eTLD+1) a host falls under, or '' if not an ad host.
// Suffix walk instead of a public-suffix list: O(labels) Set lookups.
function troveAdDomainOf(host) {
  if (!host) return '';
  const parts = host.toLowerCase().split('.');
  for (let i = Math.max(0, parts.length - 4); i < parts.length - 1; i++) {
    const suffix = parts.slice(i).join('.');
    if (TROVE_AD_DOMAINS.has(suffix)) return suffix;
  }
  return '';
}
`;

writeFileSync(OUT, body);
console.log(`wrote ${OUT}: ${domains.length} domains (${verified.length} verified seeds, ${unverified.length} unverified, ${siblings.length} EasyList siblings), ${inspect.length} inspect domains`);
