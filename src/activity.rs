//! Activity watcher: turns a stream of "what app is frontmost / is the user
//! idle" samples into merged events, and appends them files-first.
//!
//! Output is one JSONL file per local day, `activity/YYYY-MM-DD.jsonl`, one
//! merged event per line (the Trove vault spec's `activity/` stream):
//!
//! ```json
//! {"start":"2026-06-10T14:03:01-07:00","end":"2026-06-10T14:09:22-07:00",
//!  "seconds":381,"app":"Code","bundle_id":"","title":"activity.rs","afk":false}
//! ```
//!
//! AFK ("away from keyboard") spans are recorded as events with `afk: true`
//! and an empty app, so per-app totals never count idle time.
//!
//! The [`Watcher`] merge logic is deliberately free of any OS or threading so
//! it can be unit-tested with synthetic samples and timestamps; the platform
//! sampling lives in [`crate::sampler`].

use anyhow::Result;
use chrono::{DateTime, Duration, Local};
use serde::{Deserialize, Serialize};

use crate::runner::LiveCollector;
use crate::sampler::Sample;
use crate::vault::Vault;

/// Seconds between samples. Also drives the gap heuristic below.
pub const POLL_SECS: u64 = 5;

/// The live activity collector: samples the OS every poll and appends the
/// spans the [`Watcher`] state machine closes.
pub struct ActivityLive {
    watcher: Watcher,
}

impl ActivityLive {
    pub fn new() -> Self {
        ActivityLive { watcher: Watcher::new(WatchConfig::default()) }
    }
}

impl LiveCollector for ActivityLive {
    fn id(&self) -> &'static str {
        "activity"
    }

    fn tick(&mut self, vault: &Vault, now: DateTime<Local>, enabled: bool) {
        if enabled {
            let closed = self.watcher.tick(now, &crate::sampler::sample());
            if !closed.is_empty() {
                if let Err(e) = vault.append_activity_events(&closed) {
                    eprintln!("trove-collector: failed to append activity: {e:#}");
                }
            }
        } else if let Some(e) = self.watcher.flush(now) {
            // Toggled off mid-span: close out what was collected while
            // enabled rather than dropping it.
            if let Err(err) = vault.append_activity_events(&[e]) {
                eprintln!("trove-collector: failed to append activity: {err:#}");
            }
        }
    }

    fn shutdown(&mut self, vault: &Vault, now: DateTime<Local>) {
        if let Some(e) = self.watcher.flush(now) {
            if let Err(err) = vault.append_activity_events(&[e]) {
                eprintln!("trove-collector: failed to flush final activity event: {err:#}");
            }
        }
    }

    fn current(&self, now: DateTime<Local>) -> Option<ActivityEvent> {
        self.watcher.current(now)
    }
}

/// One contiguous span of using a single app/window — or being away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// RFC3339 local time, e.g. "2026-06-10T14:03:01-07:00".
    pub start: String,
    pub end: String,
    pub seconds: u64,
    /// Owning app; empty for AFK spans.
    pub app: String,
    #[serde(default)]
    pub bundle_id: String,
    #[serde(default)]
    pub title: String,
    pub afk: bool,
}

/// Tunables for [`Watcher`].
#[derive(Debug, Clone, Copy)]
pub struct WatchConfig {
    /// Idle seconds before the user counts as away.
    pub afk_threshold_secs: f64,
    /// If this many seconds pass with no sample (sleep, process paused), the
    /// open event is closed at its last-known end rather than stretched
    /// across the gap.
    pub max_gap_secs: f64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig { afk_threshold_secs: 120.0, max_gap_secs: (POLL_SECS * 6) as f64 }
    }
}

/// The currently-open (still-growing) event.
#[derive(Debug, Clone)]
struct Open {
    start: DateTime<Local>,
    end: DateTime<Local>,
    app: String,
    bundle_id: String,
    title: String,
    afk: bool,
}

impl Open {
    fn to_event(&self, end: DateTime<Local>) -> ActivityEvent {
        let end = end.max(self.start);
        ActivityEvent {
            start: self.start.to_rfc3339(),
            end: end.to_rfc3339(),
            seconds: (end - self.start).num_seconds().max(0) as u64,
            app: self.app.clone(),
            bundle_id: self.bundle_id.clone(),
            title: self.title.clone(),
            afk: self.afk,
        }
    }
}

/// Merges samples into events. Holds no OS handles — feed it `tick`s.
pub struct Watcher {
    cfg: WatchConfig,
    open: Option<Open>,
}

impl Watcher {
    pub fn new(cfg: WatchConfig) -> Self {
        Watcher { cfg, open: None }
    }

    /// Feed one sample taken at `now`. Returns any events that just *closed*
    /// (the in-progress event is held until its state changes — see
    /// [`Watcher::current`] / [`Watcher::flush`]).
    pub fn tick(&mut self, now: DateTime<Local>, sample: &Sample) -> Vec<ActivityEvent> {
        let mut closed = Vec::new();
        let afk = sample.idle_seconds >= self.cfg.afk_threshold_secs;

        // A long gap since the last sample means the machine slept or the
        // process was paused: close the open event at its last-known end and
        // don't count the gap as activity.
        let gap_end = match &self.open {
            Some(o) if (now - o.end).num_seconds() as f64 > self.cfg.max_gap_secs => Some(o.end),
            _ => None,
        };
        if let Some(end) = gap_end {
            if let Some(e) = self.close_at(end) {
                closed.push(e);
            }
        }

        let matches = self.open.as_ref().is_some_and(|o| Self::same_state(o, afk, sample));

        if matches {
            // Same activity continues — just extend its end.
            self.open.as_mut().unwrap().end = now;
        } else {
            // State changed: close the old span at the boundary, open a new one.
            // Entering AFK back-dates the boundary to when input actually
            // stopped (now - idle), so idle time isn't billed to the last app.
            let boundary = if afk {
                let idle_start = now - millis(sample.idle_seconds);
                match &self.open {
                    Some(o) => idle_start.clamp(o.start, now),
                    None => idle_start.min(now),
                }
            } else {
                now
            };
            if let Some(e) = self.close_at(boundary) {
                closed.push(e);
            }
            self.open = Some(Open {
                start: boundary,
                end: now,
                app: if afk { String::new() } else { sample.app.clone() },
                bundle_id: if afk { String::new() } else { sample.bundle_id.clone() },
                title: if afk { String::new() } else { sample.title.clone() },
                afk,
            });
        }
        closed
    }

    /// The in-progress event as of `now`, for the heartbeat's live view.
    pub fn current(&self, now: DateTime<Local>) -> Option<ActivityEvent> {
        self.open.as_ref().map(|o| o.to_event(now))
    }

    /// Close out the open event (e.g. on shutdown), if any.
    pub fn flush(&mut self, now: DateTime<Local>) -> Option<ActivityEvent> {
        self.close_at(now)
    }

    /// Take the open event, ending it at `end`. Drops zero-length spans.
    fn close_at(&mut self, end: DateTime<Local>) -> Option<ActivityEvent> {
        let o = self.open.take()?;
        let e = o.to_event(end);
        (e.seconds > 0).then_some(e)
    }

    fn same_state(o: &Open, afk: bool, s: &Sample) -> bool {
        if afk {
            o.afk
        } else {
            !o.afk && o.app == s.app && o.title == s.title
        }
    }
}

fn millis(secs: f64) -> Duration {
    Duration::milliseconds((secs * 1000.0) as i64)
}

impl Vault {
    /// Append merged events to their day's JSONL log (keyed by start day).
    pub fn append_activity_events(&self, events: &[ActivityEvent]) -> Result<()> {
        self.append_day_jsonl("activity", events, |e| &e.start, false)
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

    fn active(app: &str, title: &str) -> Sample {
        Sample { app: app.into(), bundle_id: String::new(), title: title.into(), idle_seconds: 0.0 }
    }

    fn cfg() -> WatchConfig {
        WatchConfig { afk_threshold_secs: 120.0, max_gap_secs: 30.0 }
    }

    #[test]
    fn merges_same_app_and_splits_on_change() {
        let mut w = Watcher::new(cfg());
        assert!(w.tick(at(9, 0, 0), &active("Code", "a.rs")).is_empty());
        assert!(w.tick(at(9, 0, 5), &active("Code", "a.rs")).is_empty());
        let closed = w.tick(at(9, 0, 10), &active("Safari", "Hacker News"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].app, "Code");
        assert_eq!(closed[0].seconds, 10);
        assert!(!closed[0].afk);
        let cur = w.current(at(9, 0, 15)).unwrap();
        assert_eq!(cur.app, "Safari");
        assert_eq!(cur.seconds, 5);
    }

    #[test]
    fn title_change_splits_within_same_app() {
        let mut w = Watcher::new(cfg());
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        let closed = w.tick(at(9, 0, 5), &active("Code", "b.rs"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].title, "a.rs");
    }

    #[test]
    fn afk_backdates_to_last_input() {
        let mut w = Watcher::new(cfg());
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        for ((h, m, s), idle) in [
            ((9, 0, 30), 30.0),
            ((9, 1, 0), 0.0),
            ((9, 1, 30), 30.0),
            ((9, 2, 0), 60.0),
            ((9, 2, 30), 90.0),
        ] {
            let mut sample = active("Code", "a.rs");
            sample.idle_seconds = idle;
            assert!(w.tick(at(h, m, s), &sample).is_empty());
        }
        let mut idle = active("Code", "a.rs");
        idle.idle_seconds = 120.0;
        let closed = w.tick(at(9, 3, 0), &idle);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].app, "Code");
        assert_eq!(closed[0].seconds, 60, "active span ends when input stopped");
        let afk = w.current(at(9, 3, 0)).unwrap();
        assert!(afk.afk);
        assert_eq!(afk.app, "");
        assert_eq!(afk.seconds, 120);
    }

    #[test]
    fn long_gap_closes_without_billing_the_gap() {
        let mut w = Watcher::new(cfg());
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        w.tick(at(9, 0, 5), &active("Code", "a.rs"));
        let closed = w.tick(at(9, 10, 5), &active("Code", "a.rs"));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].seconds, 5);
        assert_eq!(w.current(at(9, 10, 10)).unwrap().seconds, 5);
    }

    /// Byte-parity with the stream Trove reads: the exact line shape the
    /// app's `activity/` reader expects.
    #[test]
    fn vault_round_trip_is_byte_exact() {
        let v = temp_vault("activity");
        let mut w = Watcher::new(cfg());
        w.tick(at(9, 0, 0), &active("Code", "a.rs"));
        w.tick(at(9, 0, 30), &active("Code", "a.rs"));
        let mut events = w.tick(at(9, 1, 0), &active("Safari", "news"));
        events.extend(w.flush(at(9, 1, 30)));
        v.append_activity_events(&events).unwrap();

        let raw = std::fs::read_to_string(v.root().join("activity/2026-06-10.jsonl")).unwrap();
        let s = at(9, 0, 0).to_rfc3339();
        let e = at(9, 1, 0).to_rfc3339();
        let e2 = at(9, 1, 30).to_rfc3339();
        assert_eq!(
            raw,
            format!(
                "{{\"start\":\"{s}\",\"end\":\"{e}\",\"seconds\":60,\"app\":\"Code\",\
                 \"bundle_id\":\"\",\"title\":\"a.rs\",\"afk\":false}}\n\
                 {{\"start\":\"{e}\",\"end\":\"{e2}\",\"seconds\":30,\"app\":\"Safari\",\
                 \"bundle_id\":\"\",\"title\":\"news\",\"afk\":false}}\n"
            )
        );
        let back: Vec<ActivityEvent> = v.read_day_jsonl("activity", "2026-06-10").unwrap();
        assert_eq!(back.len(), 2);
    }
}
