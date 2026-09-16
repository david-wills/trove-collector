# trove-collector

The always-on collector for a [Trove](https://github.com/david-wills/trove) vault. A small headless Mac binary, run by `launchd`, that watches the things only a live process can see and writes them as plain files into `~/Documents/Trove`:

| Stream | What | Source |
|---|---|---|
| `activity/YYYY-MM-DD.jsonl` | which app and window is frontmost, merged into spans, with idle detection | CoreGraphics window list, every 5 s |
| `browser/YYYY-MM-DD.jsonl` | per-tab engagement spans: foreground time, background audio, favicon, referrer | the Chrome extension in `extension/`, via native messaging |
| `browser/ads/YYYY-MM-DD.jsonl` | display ads seen, with network, size, and on-screen time (extension opt-in) | same |
| `music/plays/YYYY-MM-DD.jsonl` | every Apple Music play as it happens, skips included | `com.apple.Music.playerInfo` notifications |

It follows the [vault spec](https://github.com/david-wills/trove/tree/main/docs/vault-spec) — the three streams it owns are specced at [`domains/activity.md`](https://github.com/david-wills/trove/blob/main/docs/vault-spec/domains/activity.md), [`domains/browser-visits.md`](https://github.com/david-wills/trove/blob/main/docs/vault-spec/domains/browser-visits.md), and [`domains/ads.md`](https://github.com/david-wills/trove/blob/main/docs/vault-spec/domains/ads.md); plays follow [`domains/media-plays.md`](https://github.com/david-wills/trove/blob/main/docs/vault-spec/domains/media-plays.md) — and it has **no dependency on the Trove app or its crates**. It is the reference case for the spec's central claim: any program can write to a vault by following the file conventions. The app reads these streams, shows them, and owns the on/off toggles; this binary just writes.

Why a separate program: these streams need a 24/7 process, Screen Recording permission, and a native-messaging host. Nothing else in Trove does. Keeping them here keeps the app a plain document-style app, and keeps this process small enough to audit: it is a few thousand lines and its memory is logged every ten minutes.

## Install

```bash
scripts/build.sh                  # build, sign with your Apple Development cert, install + start the launch agent
target/release/trove-collector permission   # prompt for Screen Recording (window titles; app names work without it)
target/release/trove-collector status
```

Signing matters: macOS ties the Screen Recording grant to the binary's signature, and an ad-hoc-signed build changes on every rebuild. `scripts/build.sh` signs with a stable identity so you grant once. Without a cert, plain `cargo build --release` works, but you re-grant after every build.

For the browser streams, load `extension/` unpacked in each Chrome profile (`chrome://extensions` → Developer mode → Load unpacked). The install step already registered this binary as the extension's native host. See [`extension/README.md`](extension/README.md).

Logs: `~/Library/Logs/trove/trove-collector.log` and `.err.log`. Stop and remove everything with `trove-collector uninstall`.

## How it fits with the app

- **Toggles** live in the app's Integrations hub and are written to `.trove/integrations.json`. The collector re-reads that file every poll, so a switch in the app takes effect within seconds. Ids: `activity`, `music-scrobbler`, `browser-extension`, `browser-ads`, and the default-off `browser-ads-identify`.
- **Status** is a heartbeat at `.trove/watcher-state.json` (pid, last tick, the in-progress activity event, resident memory). The app reads it (Integrations → Trove Collector, and the Activity tab) to show "collector running", the memory figure, and the live "what am I doing now" line. Cleared on clean exit; stale after a crash.
- **Single writer.** One collector per vault, enforced with an advisory lock at `.trove/watcher.lock`. A second instance waits.
- **Concurrent writers on `browser/`.** The app's history import and one native host per Chrome profile all append to the same day files, so those appends take a per-file flock. Lines never interleave.
- **The one network call.** `browser-ads-identify`, off by default, fetches Google's ad-transparency page for an observed ad to name who paid for it. Nothing else in this binary touches the network.

## Memory

The previous daemon was seen at 5–6 GB of resident memory. Two known causes are fixed here: the CoreFoundation run loop and the window-list sampler each run inside an autorelease pool, which a headless process otherwise never drains. To keep it honest the process logs `memory rss=… MB` every ten minutes and puts the same number in the heartbeat, where the app shows it. If it climbs, that's a bug, not a cost of watching.

## Layout

```
src/main.rs         CLI + signal handling; dispatches Chrome's host invocation
src/runner.rs       lock, poll loop, heartbeat, memory logging
src/vault.rs        the spec: jailed paths, day-partitioned append, toggles, heartbeat
src/activity.rs     sample → span state machine; activity/ writer
src/sampler.rs      CoreGraphics frontmost window + idle seconds
src/browser.rs      snapshot → span state machine; browser/ writer; live sidecars
src/ads.rs          ad event → record; browser/ads/ writer; opt-in identity lookup
src/music.rs        playerInfo listener + scrobbler; music/plays/ writer
src/native_host.rs  the Chrome native-messaging loop
src/launchd.rs      install / uninstall / status
extension/          the Chrome extension (MV3, no network permission)
```

Every writer has a byte-parity test against the exact line shape the app's reader expects.

## Regenerating the ad-domain list

`extension/observer/ad-domains.js` is a compiled-in allowlist of ad-serving domains. `node scripts/gen-ad-domains.mjs` rebuilds it from the public lists it cites; that script is the only thing in this repo that touches the network, and it runs on your machine, never in the extension.

## License

MIT.
