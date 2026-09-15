//! Browser watcher — the live arm of the vault's `browser/` stream.
//!
//! The Trove Chrome extension (`extension/`) snapshots the browser every few
//! seconds — the active tab of the focused window plus every audible tab —
//! and sends each snapshot over Chrome native messaging. Chrome spawns this
//! binary as the messaging host (one host process per running profile); the
//! host feeds snapshots into a [`TabTracker`], which merges them into spans
//! and appends closed spans to `browser/YYYY-MM-DD.jsonl` stamped
//! `source:"extension"`:
//!
//! ```json
//! {"time":"2026-06-10T14:03:01-07:00","url":"https://www.youtube.com/watch?v=x",
//!  "title":"…","browser":"chrome","profile":"","duration_secs":1840,
//!  "source":"extension","audible":true,"foreground_secs":95,
//!  "favicon":"https://www.youtube.com/favicon.ico","referrer":"https://www.google.com/",
//!  "transition":"link","tab_count":12}
//! ```
//!
//! The Trove app's own history import writes `source:"history"` rows into
//! the same stream; its reader resolves overlap in favour of extension rows.
//!
//! **Engagement model.** A URL has an open span while it is *engaged*: the
//! active tab of the focused browser window (foreground browsing), or audible
//! in any tab (media playback — background YouTube keeps its span open while
//! the user works elsewhere). `duration_secs` is the engaged wall time,
//! `foreground_secs` the part spent as the focused-active tab; `audible`
//! marks spans that played audio at any point.
//!
//! The tracker is an OS-free state machine: feed `(now, snapshot)`, get
//! closed spans. Snapshot gaps beyond [`ExtConfig::gap_secs`] (service
//! worker suspended, machine asleep, browser crash) close every open span at
//! its last evidence rather than stretching across the gap.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::vault::Vault;

/// Native messaging host name — the Chrome manifest filename stem and the
/// name the extension connects to. Shared with `install` (which writes the
/// manifest) so they can never drift.
pub const NATIVE_HOST_NAME: &str = "com.davidwills.trove";

/// The Trove extension's stable ID. Derived from the public key pinned in
/// `extension/manifest.json` ("key" field), so a load-unpacked install gets
/// the same ID on any machine. The native messaging manifest's
/// `allowed_origins` must list exactly this.
pub const EXTENSION_ID: &str = "inhhdcdmfoiodfkipnheoiejdegipgpb";

/// Where Chrome looks up the native messaging host manifest on macOS.
pub fn native_host_manifest_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library/Application Support/Google/Chrome/NativeMessagingHosts")
            .join(format!("{NATIVE_HOST_NAME}.json"))
    })
}

/// Seconds between extension snapshots (the extension's own cadence).
pub const EXT_SNAPSHOT_SECS: u64 = 5;

/// One visit row in `browser/YYYY-MM-DD.jsonl`. The field set and serde
/// rules match Trove's reader exactly (the vault spec's browser stream).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserVisit {
    /// RFC3339 local time, e.g. "2026-06-10T14:03:01-07:00".
    pub time: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// "chrome" (later: "safari", …).
    pub browser: String,
    #[serde(default)]
    pub profile: String,
    /// Engaged wall seconds.
    #[serde(default)]
    pub duration_secs: u64,
    /// Provenance: always "extension" for rows this collector writes.
    #[serde(default = "default_source")]
    pub source: String,
    /// The tab played audio at some point in the span.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audible: bool,
    /// Seconds spent as the active tab of the focused window.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub foreground_secs: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub favicon: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub referrer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transition: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub tab_count: u32,
}

fn default_source() -> String {
    "history".into()
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// Host of a URL: `https://www.example.com/x` → `example.com`. Non-web
/// schemes (file://, chrome://) collapse to the scheme name.
pub fn domain_of(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("", url),
    };
    if !scheme.is_empty() && scheme != "http" && scheme != "https" {
        return scheme.to_lowercase();
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        return if scheme.is_empty() { "other".into() } else { scheme.into() };
    }
    host.strip_prefix("www.").unwrap_or(host).to_lowercase()
}

/// One tab as the extension reports it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub favicon: String,
    #[serde(default)]
    pub referrer: String,
    #[serde(default)]
    pub transition: String,
}

/// One browser snapshot from the extension: who is engaged right now.
/// Unknown fields (e.g. the `"type":"snapshot"` tag) are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtSnapshot {
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub active: Option<TabInfo>,
    #[serde(default)]
    pub audible: Vec<TabInfo>,
    #[serde(default)]
    pub tab_count: u32,
}

/// One currently-open span, published for the app's "watching now" view.
/// Ephemeral — never written to the day JSONL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSpan {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub favicon: String,
    pub duration_secs: u64,
    #[serde(default)]
    pub foreground_secs: u64,
    #[serde(default)]
    pub audible: bool,
    #[serde(default)]
    pub foreground: bool,
}

/// A live sidecar: one host's open spans plus when it last wrote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveState {
    pub browser: String,
    /// Unix seconds when the host wrote this.
    pub updated: u64,
    pub spans: Vec<LiveSpan>,
}

/// Tracker tuning.
#[derive(Debug, Clone)]
pub struct ExtConfig {
    /// A silence longer than this between snapshots is a gap.
    pub gap_secs: i64,
    /// Spans shorter than this are jitter and dropped.
    pub min_span_secs: i64,
}

impl Default for ExtConfig {
    fn default() -> Self {
        Self { gap_secs: (EXT_SNAPSHOT_SECS * 4) as i64, min_span_secs: 1 }
    }
}

struct OpenSpan {
    start: DateTime<Local>,
    last_seen: DateTime<Local>,
    title: String,
    favicon: String,
    referrer: String,
    transition: String,
    tab_count: u32,
    foreground_secs: i64,
    /// Was this the focused-active tab at the previous snapshot? The elapsed
    /// interval is billed to the *previous* state.
    fg_last: bool,
    audible_any: bool,
}

struct Engaged<'a> {
    title: &'a str,
    favicon: &'a str,
    referrer: &'a str,
    transition: &'a str,
    fg: bool,
    audible: bool,
}

/// The snapshot→span merge state machine. Holds no OS handles; the native
/// messaging host owns the I/O and feeds this.
pub struct TabTracker {
    config: ExtConfig,
    browser: String,
    spans: HashMap<String, OpenSpan>,
    last_tick: Option<DateTime<Local>>,
}

impl TabTracker {
    pub fn new(browser: &str, config: ExtConfig) -> Self {
        Self { config, browser: browser.to_string(), spans: HashMap::new(), last_tick: None }
    }

    /// Fold one snapshot in; returns spans that just closed.
    pub fn tick(&mut self, now: DateTime<Local>, snap: &ExtSnapshot) -> Vec<BrowserVisit> {
        let mut closed = Vec::new();
        let mut dt = self.last_tick.map(|p| (now - p).num_seconds()).unwrap_or(0);
        if dt > self.config.gap_secs || dt < 0 {
            for (url, s) in std::mem::take(&mut self.spans) {
                let end = s.last_seen;
                closed.extend(self.close(url, s, end));
            }
            dt = 0;
        }
        for s in self.spans.values_mut() {
            if s.fg_last {
                s.foreground_secs += dt;
            }
        }
        let mut engaged: HashMap<&str, Engaged> = HashMap::new();
        if snap.focused {
            if let Some(a) = &snap.active {
                if !a.url.is_empty() {
                    engaged.insert(
                        &a.url,
                        Engaged {
                            title: &a.title,
                            favicon: &a.favicon,
                            referrer: &a.referrer,
                            transition: &a.transition,
                            fg: true,
                            audible: false,
                        },
                    );
                }
            }
        }
        for t in &snap.audible {
            if t.url.is_empty() {
                continue;
            }
            engaged
                .entry(&t.url)
                .and_modify(|e| {
                    e.audible = true;
                    if e.favicon.is_empty() {
                        e.favicon = &t.favicon;
                    }
                })
                .or_insert(Engaged {
                    title: &t.title,
                    favicon: &t.favicon,
                    referrer: "",
                    transition: "",
                    fg: false,
                    audible: true,
                });
        }
        for (url, e) in &engaged {
            match self.spans.entry(url.to_string()) {
                Entry::Occupied(mut occ) => {
                    let s = occ.get_mut();
                    s.last_seen = now;
                    s.fg_last = e.fg;
                    s.audible_any |= e.audible;
                    if !e.title.is_empty() {
                        s.title = e.title.to_string();
                    }
                    if !e.favicon.is_empty() {
                        s.favicon = e.favicon.to_string();
                    }
                }
                Entry::Vacant(v) => {
                    v.insert(OpenSpan {
                        start: now,
                        last_seen: now,
                        title: e.title.to_string(),
                        favicon: e.favicon.to_string(),
                        referrer: e.referrer.to_string(),
                        transition: e.transition.to_string(),
                        tab_count: snap.tab_count,
                        foreground_secs: 0,
                        fg_last: e.fg,
                        audible_any: e.audible,
                    });
                }
            }
        }
        let gone: Vec<String> =
            self.spans.keys().filter(|u| !engaged.contains_key(u.as_str())).cloned().collect();
        for url in gone {
            let s = self.spans.remove(&url).expect("key from spans");
            closed.extend(self.close(url, s, now));
        }
        self.last_tick = Some(now);
        closed
    }

    /// Close out everything at its last evidence — on shutdown (the browser
    /// disconnecting the host).
    pub fn flush(&mut self) -> Vec<BrowserVisit> {
        let mut closed = Vec::new();
        for (url, s) in std::mem::take(&mut self.spans) {
            let end = s.last_seen;
            closed.extend(self.close(url, s, end));
        }
        self.last_tick = None;
        closed
    }

    /// The currently-open spans with elapsed durations against `now`. A pure
    /// read for the live sidecar; never appended to the day JSONL.
    pub fn live(&self, now: DateTime<Local>) -> Vec<LiveSpan> {
        let pending = self.last_tick.map(|p| (now - p).num_seconds().max(0)).unwrap_or(0);
        self.spans
            .iter()
            .map(|(url, s)| {
                let secs = (now - s.start).num_seconds().max(0);
                let fg = s.foreground_secs + if s.fg_last { pending } else { 0 };
                LiveSpan {
                    url: url.clone(),
                    title: s.title.clone(),
                    favicon: s.favicon.clone(),
                    duration_secs: secs as u64,
                    foreground_secs: fg.min(secs) as u64,
                    audible: s.audible_any,
                    foreground: s.fg_last,
                }
            })
            .collect()
    }

    fn close(&self, url: String, s: OpenSpan, end: DateTime<Local>) -> Option<BrowserVisit> {
        let secs = (end - s.start).num_seconds();
        if secs < self.config.min_span_secs {
            return None;
        }
        Some(BrowserVisit {
            time: s.start.to_rfc3339(),
            url,
            title: s.title,
            browser: self.browser.clone(),
            profile: String::new(),
            duration_secs: secs as u64,
            source: "extension".into(),
            audible: s.audible_any,
            foreground_secs: s.foreground_secs.clamp(0, secs) as u64,
            favicon: s.favicon,
            referrer: s.referrer,
            transition: s.transition,
            tab_count: s.tab_count,
        })
    }
}

impl Vault {
    /// Append closed spans to `browser/YYYY-MM-DD.jsonl`. The stream has
    /// several legitimate writers (one host per Chrome profile, plus the
    /// app's history import), so each day file is flocked while appending.
    pub fn append_browser_visits(&self, visits: &[BrowserVisit]) -> Result<()> {
        self.append_day_jsonl("browser", visits, |v| &v.time, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::temp_vault;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 10, h, m, s).unwrap()
    }

    fn tab(url: &str, title: &str) -> TabInfo {
        TabInfo { url: url.into(), title: title.into(), ..Default::default() }
    }

    fn fg(url: &str, title: &str) -> ExtSnapshot {
        ExtSnapshot { focused: true, active: Some(tab(url, title)), audible: vec![], ..Default::default() }
    }

    fn tracker() -> TabTracker {
        TabTracker::new("chrome", ExtConfig::default())
    }

    #[test]
    fn foreground_span_extends_and_closes_on_url_change() {
        let mut t = tracker();
        assert!(t.tick(at(10, 0, 0), &fg("https://a.com/", "A")).is_empty());
        assert!(t.tick(at(10, 0, 5), &fg("https://a.com/", "A")).is_empty());
        let closed = t.tick(at(10, 0, 10), &fg("https://b.com/", "B"));
        assert_eq!(closed.len(), 1);
        let v = &closed[0];
        assert_eq!(v.url, "https://a.com/");
        assert_eq!(v.duration_secs, 10);
        assert_eq!(v.foreground_secs, 10);
        assert_eq!(v.source, "extension");
        assert!(!v.audible);
    }

    #[test]
    fn background_audible_tab_tracks_without_foreground_time() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: true,
            active: Some(tab("https://docs.example/", "Doc")),
            audible: vec![tab("https://www.youtube.com/watch?v=x", "Song")],
            ..Default::default()
        };
        t.tick(at(12, 0, 0), &snap);
        t.tick(at(12, 0, 5), &snap);
        let closed = t.tick(at(12, 0, 10), &fg("https://docs.example/", "Doc"));
        assert_eq!(closed.len(), 1);
        assert!(closed[0].audible);
        assert_eq!(closed[0].duration_secs, 10);
        assert_eq!(closed[0].foreground_secs, 0);
    }

    #[test]
    fn unfocused_active_tab_is_not_engaged() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: false,
            active: Some(tab("https://a.com/", "A")),
            audible: vec![],
            ..Default::default()
        };
        t.tick(at(8, 0, 0), &snap);
        t.tick(at(8, 0, 5), &snap);
        assert!(t.flush().is_empty());
    }

    #[test]
    fn gap_closes_at_last_evidence_not_across_it() {
        let mut t = tracker();
        t.tick(at(10, 0, 0), &fg("https://a.com/", "A"));
        t.tick(at(10, 0, 5), &fg("https://a.com/", "A"));
        let closed = t.tick(at(10, 10, 5), &fg("https://a.com/", "A"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].duration_secs, 5);
    }

    #[test]
    fn live_reports_open_spans_without_closing_them() {
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: true,
            active: Some(tab("https://www.youtube.com/watch?v=x", "Video")),
            audible: vec![tab("https://www.youtube.com/watch?v=x", "Video")],
            ..Default::default()
        };
        for i in 0..=6 {
            assert!(t.tick(at(9, 0, 0) + chrono::Duration::seconds(i * 5), &snap).is_empty());
        }
        let live = t.live(at(9, 0, 30));
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].duration_secs, 30);
        assert!(live[0].audible && live[0].foreground);
        assert!(t.tick(at(9, 0, 35), &snap).is_empty());
    }

    #[test]
    fn snapshot_json_shape_from_extension_parses() {
        let snap: ExtSnapshot = serde_json::from_str(
            r#"{"type":"snapshot","focused":true,"tab_count":7,
                "active":{"url":"https://a.com/","title":"A","favicon":"https://a.com/f.ico",
                          "referrer":"https://ref.com/","transition":"link"},
                "audible":[{"url":"https://b.com/","title":"B"}]}"#,
        )
        .unwrap();
        assert!(snap.focused);
        assert_eq!(snap.tab_count, 7);
        assert_eq!(snap.active.unwrap().referrer, "https://ref.com/");
    }

    #[test]
    fn domain_extraction() {
        assert_eq!(domain_of("https://www.Example.com/x?y"), "example.com");
        assert_eq!(domain_of("https://user@host.io:8080/"), "host.io");
        assert_eq!(domain_of("chrome://extensions"), "chrome");
        assert_eq!(domain_of(""), "other");
    }

    /// Byte-parity with Trove's browser reader: false/zero extras are not
    /// serialized, the field order is fixed.
    #[test]
    fn append_writes_byte_identical_jsonl() {
        let v = temp_vault("browser");
        let mut t = tracker();
        let snap = ExtSnapshot {
            focused: true,
            tab_count: 9,
            active: Some(TabInfo {
                url: "https://a.com/".into(),
                title: "A".into(),
                favicon: "https://a.com/f.ico".into(),
                referrer: "https://google.com/".into(),
                transition: "link".into(),
            }),
            audible: vec![],
        };
        t.tick(at(10, 0, 0), &snap);
        t.tick(at(10, 0, 5), &snap);
        let closed = t.tick(at(10, 0, 10), &fg("https://b.com/", "B"));
        v.append_browser_visits(&closed).unwrap();
        let raw = std::fs::read_to_string(v.root().join("browser/2026-06-10.jsonl")).unwrap();
        let s = at(10, 0, 0).to_rfc3339();
        assert_eq!(
            raw,
            format!(
                "{{\"time\":\"{s}\",\"url\":\"https://a.com/\",\"title\":\"A\",\"browser\":\"chrome\",\
                 \"profile\":\"\",\"duration_secs\":10,\"source\":\"extension\",\"foreground_secs\":10,\
                 \"favicon\":\"https://a.com/f.ico\",\"referrer\":\"https://google.com/\",\
                 \"transition\":\"link\",\"tab_count\":9}}\n"
            )
        );
    }
}
