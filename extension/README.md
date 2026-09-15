# Trove Browser Watcher extension

The live half of Trove's browser collector: snapshots the active tab and
every audible tab a few times a minute and streams them to the local `trove-collector`
binary over Chrome native messaging. trove-collector merges snapshots into spans and
appends them to `~/Documents/Trove/browser/YYYY-MM-DD.jsonl` with `source:"extension"`
— richer and authoritative over the history import (`source:"history"`),
which keeps running as backup. **Local-only:** no host permissions, no
network access; data goes binary-to-binary on this machine.

Why an extension at all: it is the only complete way to capture background
media (YouTube playing while another app is focused, via `tabs.Tab.audible`)
and real per-tab engagement spans, with no retention limit.

## Install (Chrome, unpacked — David-first workflow)

1. Build and register the host (writes the native messaging manifest):

   ```bash
   scripts/build.sh
   target/release/trove-collector status
   ```

2. `chrome://extensions` → enable **Developer mode** → **Load unpacked** →
   select this `extension/` directory.
3. Repeat step 2 in each Chrome profile you want tracked (one host process
   runs per profile; spans can't carry the profile name — the tabs API
   doesn't expose it — so extension rows have an empty `profile`).
4. Verify: browse for ~30 seconds, then check
   `tail ~/Documents/Trove/browser/$(date +%F).jsonl` for `"source":"extension"` rows.

The `key` field in `manifest.json` pins the extension ID to
`inhhdcdmfoiodfkipnheoiejdegipgpb` on any machine (the ID is a hash of that
public key), which is what the native messaging manifest's `allowed_origins`
references. Don't change the key — Chrome would derive a new ID and the host
would refuse the connection.

## Event model

- One JSON snapshot every 5s while connected, plus immediately (debounced
  200ms) on tab activation/removal, URL/title/audible changes, and window
  focus changes: `{focused, active: {url,title}|null, audible: [{url,title}]}`.
- A URL is *engaged* while it's the active tab of the focused window or
  audible anywhere. Span rows record total engaged time (`duration_secs`),
  focused-active time (`foreground_secs`), and whether audio ever played
  (`audible`) — so background playback is attributable but never billed as
  focus time. trove-collector also gates `focused` on system idle (≥2 min → not
  engaged), which the extension can't see.
- If trove-collector isn't installed the extension idles and retries every 30s; no
  data is recorded (the history import still catches those visits later).

## Page observer (opt-in, off by default)

A second arm behind `chrome://extensions → Details → Extension options`
(spec: `docs/page-observer-spec.md`). With everything off — the default —
the extension is exactly the dumb watcher above: no host permissions
("Site access: none"), no content scripts, no page reading.

- **Page observation** (master): enabling calls Chrome's own permission
  prompt for site access; disabling removes the grant, so "off" is enforced
  by Chrome, not by our code. Chrome's per-site access controls work on top.
- **Ad observation** (first feature): records which display ads were served
  (network, advertiser via click-through landing URL, size) and how long
  each was ≥50% on screen (`viewable` = the MRC 50%/1s standard) to
  `~/Documents/Trove/browser/ads/YYYY-MM-DD.jsonl`. Never blocks ads, never captures
  page text or creative images.
- Detection matches iframes against `observer/ad-domains.js`, a generated
  module (regenerate with `node scripts/gen-ad-domains.mjs` and commit) —
  no runtime fetches, no filter-list engine.
- Vault-side, the `browser-ads` integrations-hub toggle gates appends
  independently (defense in depth).
- Manual smoke: serve `test/fixtures/ads.html` over http (see comments in
  the file), scroll/switch tabs, close the tab, check the vault rows.

## Safari (later)

Safari 14+ runs the same WebExtension code, but it must be wrapped in a
native app via Xcode (Trove.app can host it) and its `onUpdated` audible
event is reportedly flaky — the 5s polling cadence doubles as the fallback.
See `docs/data-sources.md` §2.
