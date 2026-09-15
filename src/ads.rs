//! Ad observation stream — the extension's opt-in page observer. Off by
//! default twice over: the extension only observes after a Chrome-mediated
//! permission grant, and the host only appends while the `browser-ads`
//! integration is enabled in the Trove app.
//!
//! One JSONL file per local day, keyed by the day the ad record *closed*:
//! `browser/ads/YYYY-MM-DD.jsonl`, one record per line:
//!
//! ```json
//! {"ts":"2026-06-11T10:23:45-07:00","end":"2026-06-11T10:24:10-07:00",
//!  "page_url":"https://example.com/article",
//!  "frame_url":"https://googleads.g.doubleclick.net/...",
//!  "landing_url":"https://advertiser.com/promo","network":"doubleclick.net",
//!  "advertiser":"advertiser.com","viewed_secs":12.5,"viewable":true,
//!  "w":300,"h":250,"source":"extension"}
//! ```
//!
//! The extension is a dumb sensor shipping raw URLs and epoch timestamps
//! ([`AdEvent`]); `network`/`advertiser` are derived here at append time.
//! `viewable` follows the MRC display standard: ≥50% of pixels in the
//! viewport for ≥1 continuous second.
//!
//! The opt-in `browser-ads-identify` resolver is the one networked path in
//! this whole binary: when on, it fetches Google's ad-transparency page named
//! by the creative's own AdChoices link and reads the "Paid for by" line.

use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::domain_of;
use crate::vault::Vault;

/// One closed ad record as the extension ships it (wire shape, inside a
/// `{type:"ads", events:[…]}` native message).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdEvent {
    #[serde(default)]
    pub ts_ms: i64,
    #[serde(default)]
    pub end_ms: i64,
    #[serde(default)]
    pub page_url: String,
    #[serde(default)]
    pub frame_url: String,
    #[serde(default)]
    pub landing_url: String,
    /// Matched ad-slot name prefix — the network fallback when `frame_url`
    /// is empty (srcdoc ads).
    #[serde(default)]
    pub slot: String,
    #[serde(default)]
    pub viewed_secs: f64,
    #[serde(default)]
    pub viewable: bool,
    #[serde(default)]
    pub w: u32,
    #[serde(default)]
    pub h: u32,
    /// Google's "Why this ad?" transparency URL, if present. Wire-only,
    /// never stored; consumed by the opt-in resolver.
    #[serde(default)]
    pub why_url: String,
}

/// One stored ad record (a line in `browser/ads/YYYY-MM-DD.jsonl`). Field
/// set and serde rules match Trove's reader exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdRecord {
    pub ts: String,
    pub end: String,
    pub page_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub frame_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub landing_url: String,
    pub network: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub advertiser: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub advertiser_id: String,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub viewed_secs: f64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub viewable: bool,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub w: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub h: u32,
    #[serde(default = "ext_source")]
    pub source: String,
}

fn ext_source() -> String {
    "extension".into()
}

fn is_zero_f64(n: &f64) -> bool {
    *n == 0.0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

/// Slot-prefix → network family, mirroring the extension's
/// `TROVE_AD_SLOT_PREFIXES` (all current slot patterns are Google's).
const SLOT_NETWORKS: &[(&str, &str)] =
    &[("google_ads_iframe", "google"), ("aswift_", "google"), ("div-gpt-ad", "google")];

/// Two-part public suffixes seen on ad/advertiser domains, so eTLD+1
/// reduction doesn't mangle "foo.co.uk" into "co.uk".
const TWO_PART_SUFFIXES: &[&str] =
    &["co.uk", "com.au", "co.jp", "co.kr", "com.br", "co.in", "com.tr", "net.au", "co.nz"];

/// Registrable domain (eTLD+1-ish) of a URL.
fn registrable_of(url: &str) -> String {
    let host = domain_of(url);
    if !host.contains('.') {
        return String::new();
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let two = labels[labels.len() - 2..].join(".");
    let take = if TWO_PART_SUFFIXES.contains(&two.as_str()) { 3 } else { 2 };
    labels[labels.len() - take..].join(".")
}

fn network_of(frame_url: &str, slot: &str) -> String {
    let from_frame = registrable_of(frame_url);
    if !from_frame.is_empty() {
        return from_frame;
    }
    for (prefix, network) in SLOT_NETWORKS {
        if slot.starts_with(prefix) {
            return (*network).into();
        }
    }
    "unknown".into()
}

fn ms_to_local(ms: i64) -> Option<DateTime<Local>> {
    DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&Local))
}

// ── Advertiser identity resolution (the `browser-ads-identify` opt-in) ──────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AdIdentity {
    /// "Paid for by" display name. Empty = resolved but unattributable
    /// (cached negatively so the page isn't refetched).
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ad_id: String,
}

const WTA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
const WTA_UA: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// Cap on transparency fetches per ad batch.
const MAX_RESOLVE_PER_BATCH: usize = 25;
/// Vault-relative cache of resolved identities, keyed by [`why_token`].
const IDENTITY_CACHE: &str = "browser/ads/.identities.json";

/// Stable cache key for a transparency URL: its `reasons` token if present,
/// else the whole URL.
fn why_token(why_url: &str) -> String {
    match why_url.find("reasons=") {
        Some(i) => {
            let rest = &why_url[i + "reasons=".len()..];
            let tok = rest.split('&').next().unwrap_or(rest);
            if tok.is_empty() {
                why_url.to_string()
            } else {
                tok.to_string()
            }
        }
        None => why_url.to_string(),
    }
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Parse the advertiser identity out of a transparency page's HTML.
fn parse_identity(html: &str) -> Option<AdIdentity> {
    let name = html_unescape(between(html, "Paid for by ", "<")?.trim());
    if name.is_empty() {
        return None;
    }
    let ad_id = between(html, "adstransparency.google.com/advertiser/", "?")
        .or_else(|| between(html, "adstransparency.google.com/advertiser/", "\""))
        .map(|s| s.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>())
        .filter(|s| s.starts_with("AR"))
        .unwrap_or_default();
    Some(AdIdentity { name, ad_id })
}

/// Fetch and parse one transparency page. Any failure is `None` —
/// best-effort enrichment, never a gate.
fn fetch_identity(why_url: &str) -> Option<AdIdentity> {
    let body = ureq::get(why_url)
        .timeout(WTA_TIMEOUT)
        .set("User-Agent", WTA_UA)
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_identity(&body)
}

impl AdEvent {
    /// Derive the stored record, sanity-bounding timestamps against the
    /// host's arrival time (a wrong browser clock must not file records in
    /// the future).
    fn into_record(&self, received: DateTime<Local>) -> AdRecord {
        let end = ms_to_local(self.end_ms).unwrap_or(received).min(received);
        let ts = ms_to_local(self.ts_ms).unwrap_or(end).min(end);
        let span_secs = (end - ts).num_milliseconds() as f64 / 1000.0;
        AdRecord {
            ts: ts.to_rfc3339(),
            end: end.to_rfc3339(),
            page_url: self.page_url.clone(),
            frame_url: self.frame_url.clone(),
            landing_url: self.landing_url.clone(),
            network: network_of(&self.frame_url, &self.slot),
            advertiser: registrable_of(&self.landing_url),
            advertiser_id: String::new(),
            viewed_secs: self.viewed_secs.clamp(0.0, span_secs.max(0.0)),
            viewable: self.viewable,
            w: self.w,
            h: self.h,
            source: ext_source(),
        }
    }
}

impl Vault {
    /// Derive and append ad records, optionally resolving each advertiser's
    /// identity off Google's transparency page first (`resolve` mirrors the
    /// `browser-ads-identify` opt-in).
    pub fn ingest_ad_events(
        &self,
        events: &[AdEvent],
        received: DateTime<Local>,
        resolve: bool,
    ) -> Result<()> {
        let mut records: Vec<AdRecord> = events.iter().map(|e| e.into_record(received)).collect();
        if resolve {
            self.resolve_advertisers(&mut records, events);
        }
        // Keyed by the *close* day; flocked because one host runs per
        // Chrome profile.
        self.append_day_jsonl("browser/ads", &records, |r| &r.end, true)
    }

    fn resolve_advertisers(&self, records: &mut [AdRecord], events: &[AdEvent]) {
        let path = self.root().join(IDENTITY_CACHE);
        let mut cache: BTreeMap<String, AdIdentity> = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut changed = false;
        let mut fetched = 0usize;
        for (rec, ev) in records.iter_mut().zip(events) {
            // A landing-derived advertiser is the clearer signal — keep it.
            if ev.why_url.is_empty() || !rec.advertiser.is_empty() {
                continue;
            }
            let key = why_token(&ev.why_url);
            let ident = match cache.get(&key) {
                Some(c) => c.clone(),
                None if fetched < MAX_RESOLVE_PER_BATCH => {
                    fetched += 1;
                    let got = fetch_identity(&ev.why_url).unwrap_or_default();
                    cache.insert(key, got.clone());
                    changed = true;
                    got
                }
                None => continue,
            };
            if !ident.name.is_empty() {
                rec.advertiser = ident.name;
                rec.advertiser_id = ident.ad_id;
            }
        }
        if changed {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(s) = serde_json::to_string_pretty(&cache) {
                let _ = fs::write(&path, s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::temp_vault;
    use chrono::TimeZone;

    fn ms(d: u32, h: u32, m: u32) -> i64 {
        Local.with_ymd_and_hms(2026, 6, d, h, m, 0).unwrap().timestamp_millis()
    }

    fn event(d: u32, h: u32, frame: &str, landing: &str, viewed: f64) -> AdEvent {
        AdEvent {
            ts_ms: ms(d, h, 0),
            end_ms: ms(d, h, 2),
            page_url: "https://example.com/article".into(),
            frame_url: frame.into(),
            landing_url: landing.into(),
            slot: String::new(),
            viewed_secs: viewed,
            viewable: viewed >= 1.0,
            w: 300,
            h: 250,
            why_url: String::new(),
        }
    }

    fn received() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 12, 0, 0, 0).unwrap()
    }

    #[test]
    fn registrable_reduction() {
        assert_eq!(registrable_of("https://googleads.g.doubleclick.net/pagead/x"), "doubleclick.net");
        assert_eq!(registrable_of("https://shop.example.co.uk/p"), "example.co.uk");
        assert_eq!(registrable_of("about:blank"), "");
    }

    #[test]
    fn network_falls_back_to_slot() {
        assert_eq!(network_of("", "google_ads_iframe"), "google");
        assert_eq!(network_of("", ""), "unknown");
    }

    /// Byte-parity with Trove's ads reader.
    #[test]
    fn append_writes_byte_identical_jsonl() {
        let v = temp_vault("ads-parity");
        let full = event(10, 9, "https://googleads.g.doubleclick.net/x", "https://www.advertiser.com/promo", 12.5);
        let bare = event(10, 9, "", "", 0.0);
        v.ingest_ad_events(&[full, bare.clone()], received(), false).unwrap();
        v.ingest_ad_events(&[bare], received(), false).unwrap();

        let ts = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap().to_rfc3339();
        let end = Local.with_ymd_and_hms(2026, 6, 10, 9, 2, 0).unwrap().to_rfc3339();
        let full_line = format!(
            "{{\"ts\":\"{ts}\",\"end\":\"{end}\",\"page_url\":\"https://example.com/article\",\
             \"frame_url\":\"https://googleads.g.doubleclick.net/x\",\
             \"landing_url\":\"https://www.advertiser.com/promo\",\
             \"network\":\"doubleclick.net\",\"advertiser\":\"advertiser.com\",\
             \"viewed_secs\":12.5,\"viewable\":true,\"w\":300,\"h\":250,\"source\":\"extension\"}}"
        );
        let bare_line = format!(
            "{{\"ts\":\"{ts}\",\"end\":\"{end}\",\"page_url\":\"https://example.com/article\",\
             \"network\":\"unknown\",\"w\":300,\"h\":250,\"source\":\"extension\"}}"
        );
        let raw = fs::read_to_string(v.root().join("browser/ads/2026-06-10.jsonl")).unwrap();
        assert_eq!(raw, format!("{full_line}\n{bare_line}\n{bare_line}\n"));
    }

    #[test]
    fn timestamps_are_sanity_bounded() {
        let v = temp_vault("ads-bounds");
        let mut e = event(10, 9, "https://googleads.g.doubleclick.net/x", "", 9999.0);
        e.end_ms = ms(13, 9, 0);
        v.ingest_ad_events(&[e], received(), false).unwrap();
        let rows: Vec<AdRecord> = v.read_day_jsonl("browser/ads", "2026-06-12").unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn parse_identity_from_real_transparency_page() {
        let html = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ads/whythisad.html")).unwrap();
        let id = parse_identity(&html).expect("names a payer");
        assert_eq!(id.name, "Hearts & Science LLC");
        assert_eq!(id.ad_id, "AR04055566952792326145");
        assert!(parse_identity("<html><body>About this ad</body></html>").is_none());
    }

    #[test]
    fn resolve_overlays_cached_identity_without_network() {
        let v = temp_vault("ads-resolve");
        let dir = v.root().join("browser/ads");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".identities.json"), r#"{"TOK42":{"name":"Hearts & Science LLC","ad_id":"AR0405"}}"#).unwrap();
        let mut e = event(10, 9, "https://googleads.g.doubleclick.net/x", "", 2.0);
        e.why_url = "https://adssettings.google.com/whythisad?source=display&reasons=TOK42".into();
        v.ingest_ad_events(&[e], received(), true).unwrap();
        let rows: Vec<AdRecord> = v.read_day_jsonl("browser/ads", "2026-06-10").unwrap();
        assert_eq!(rows[0].advertiser, "Hearts & Science LLC");
        assert_eq!(rows[0].advertiser_id, "AR0405");
        assert_eq!(why_token("https://x/whythisad?source=display&reasons=ABC&x=1"), "ABC");
    }
}
