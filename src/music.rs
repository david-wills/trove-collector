//! Apple Music play history (scrobbler).
//!
//! Music.app posts a `com.apple.Music.playerInfo` distributed notification on
//! every play/pause/stop/track change, but keeps no play *history* itself —
//! this stream is unrecoverable unless captured live.
//!
//! One JSONL file per local day, `music/plays/YYYY-MM-DD.jsonl`, one
//! completed play per line:
//!
//! ```json
//! {"start":"2026-06-10T20:03:01-07:00","end":"2026-06-10T20:06:50-07:00",
//!  "seconds_played":228,"track":"Take On Me","artist":"a-ha",
//!  "album":"Hunting High & Low","genre":"Pop","duration_secs":228.7,
//!  "persistent_id":"DAE7C16E62517F1A","full_play":true}
//! ```
//!
//! Every play longer than a tiny anti-flicker floor (5s) is recorded — skips
//! included. The Last.fm-style judgment (half the track or 4 minutes) is
//! stored as `full_play`; readers can apply any other rule later.
//!
//! ## The main-run-loop contract
//!
//! Distributed notifications are delivered on the process's **main** run
//! loop, so the host must keep it running: [`pump_main_run_loop`] on the
//! main thread, with the collector loop on a worker. Each run-loop slice is
//! wrapped in an autorelease pool — without it a headless process leaks
//! every autoreleased CFString the run loop touches (observed once as
//! ~2.9 GB over three days).

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::activity::ActivityEvent;
use crate::runner::LiveCollector;
use crate::vault::Vault;

/// Track metadata carried by a playerInfo notification.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub name: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub duration_secs: Option<f64>,
    /// Music's persistent ID as uppercase hex; empty if absent.
    pub persistent_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Playing,
    Paused,
    Stopped,
}

/// One playerInfo notification, decoded. A track change arrives as a bare
/// `Stopped` (no track) followed by `Playing` with the new track.
#[derive(Debug, Clone)]
pub struct PlayerEvent {
    pub state: PlayerState,
    pub track: Option<TrackInfo>,
}

/// One completed play (full listen or skip). Field set and serde rules
/// match Trove's reader exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Play {
    pub start: String,
    pub end: String,
    pub seconds_played: u64,
    pub track: String,
    pub artist: String,
    pub album: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub persistent_id: String,
    #[serde(default)]
    pub full_play: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ScrobbleConfig {
    pub min_record_secs: f64,
    pub full_fraction: f64,
    pub full_secs: f64,
}

const UNKNOWN_DURATION_FULL_SECS: f64 = 30.0;

impl Default for ScrobbleConfig {
    fn default() -> Self {
        ScrobbleConfig { min_record_secs: 5.0, full_fraction: 0.5, full_secs: 240.0 }
    }
}

#[derive(Debug, Clone)]
struct OpenPlay {
    track: TrackInfo,
    start: DateTime<Local>,
    played: f64,
    resumed_at: Option<DateTime<Local>>,
    last_active: DateTime<Local>,
}

/// Folds playerInfo events into completed plays. Holds no OS handles.
pub struct Scrobbler {
    cfg: ScrobbleConfig,
    open: Option<OpenPlay>,
}

impl Scrobbler {
    pub fn new(cfg: ScrobbleConfig) -> Self {
        Scrobbler { cfg, open: None }
    }

    pub fn handle(&mut self, now: DateTime<Local>, event: &PlayerEvent) -> Option<Play> {
        match event.state {
            PlayerState::Playing => {
                let track = event.track.as_ref()?;
                if self.open.as_ref().is_some_and(|o| same_track(&o.track, track)) {
                    let o = self.open.as_mut().unwrap();
                    if o.resumed_at.is_none() {
                        o.resumed_at = Some(now);
                    }
                    o.last_active = now;
                    None
                } else {
                    let done = self.close(now);
                    self.open = Some(OpenPlay {
                        track: track.clone(),
                        start: now,
                        played: 0.0,
                        resumed_at: Some(now),
                        last_active: now,
                    });
                    done
                }
            }
            PlayerState::Paused => {
                if let Some(o) = self.open.as_mut() {
                    if let Some(r) = o.resumed_at.take() {
                        o.played += secs_between(r, now);
                        o.last_active = now;
                    }
                }
                None
            }
            PlayerState::Stopped => self.close(now),
        }
    }

    pub fn flush(&mut self, now: DateTime<Local>) -> Option<Play> {
        self.close(now)
    }

    fn close(&mut self, now: DateTime<Local>) -> Option<Play> {
        let mut o = self.open.take()?;
        if let Some(r) = o.resumed_at.take() {
            o.played += secs_between(r, now);
            o.last_active = now;
        }
        if o.track.name.is_empty() || o.played < self.cfg.min_record_secs {
            return None;
        }
        Some(Play {
            start: o.start.to_rfc3339(),
            end: o.last_active.to_rfc3339(),
            seconds_played: o.played.round() as u64,
            full_play: self.is_full(&o),
            track: o.track.name,
            artist: o.track.artist,
            album: o.track.album,
            genre: o.track.genre,
            duration_secs: o.track.duration_secs,
            persistent_id: o.track.persistent_id,
        })
    }

    fn is_full(&self, o: &OpenPlay) -> bool {
        match o.track.duration_secs {
            Some(d) => o.played >= d * self.cfg.full_fraction || o.played >= self.cfg.full_secs,
            None => o.played >= UNKNOWN_DURATION_FULL_SECS,
        }
    }
}

fn same_track(a: &TrackInfo, b: &TrackInfo) -> bool {
    if !a.persistent_id.is_empty() && !b.persistent_id.is_empty() {
        a.persistent_id == b.persistent_id
    } else {
        a.name == b.name && a.artist == b.artist && a.album == b.album
    }
}

fn secs_between(from: DateTime<Local>, to: DateTime<Local>) -> f64 {
    ((to - from).num_milliseconds() as f64 / 1000.0).max(0.0)
}

impl Vault {
    /// Append completed plays to their day's JSONL log (keyed by start day).
    pub fn append_music_plays(&self, plays: &[Play]) -> Result<()> {
        self.append_day_jsonl("music/plays", plays, |p| &p.start, false)
    }
}

// ── the live collector ──────────────────────────────────────────────────────

/// The live scrobbler: owns the playerInfo notification channel and the
/// [`Scrobbler`] state machine.
pub struct MusicLive {
    listener: Option<MusicListener>,
    rx: Receiver<TimedPlayerEvent>,
    scrobbler: Scrobbler,
}

impl MusicLive {
    pub fn new() -> Self {
        let (listener, rx) = MusicListener::start();
        MusicLive { listener: Some(listener), rx, scrobbler: Scrobbler::new(ScrobbleConfig::default()) }
    }

    fn append(&self, vault: &Vault, plays: &[Play], context: &str) {
        if !plays.is_empty() {
            if let Err(e) = vault.append_music_plays(plays) {
                eprintln!("trove-collector: failed to {context} music plays: {e:#}");
            }
        }
    }
}

impl LiveCollector for MusicLive {
    fn id(&self) -> &'static str {
        "music-scrobbler"
    }

    fn tick(&mut self, vault: &Vault, now: DateTime<Local>, enabled: bool) {
        let mut plays = Vec::new();
        if enabled {
            while let Ok((ts, ev)) = self.rx.try_recv() {
                plays.extend(self.scrobbler.handle(ts, &ev));
            }
        } else {
            // Disabled: discard new events (re-enabling must not replay a
            // backlog of stale player state), close out any open play.
            while self.rx.try_recv().is_ok() {}
            plays.extend(self.scrobbler.flush(now));
        }
        self.append(vault, &plays, "append");
    }

    fn shutdown(&mut self, vault: &Vault, now: DateTime<Local>) {
        if let Some(listener) = self.listener.take() {
            listener.stop();
        }
        let mut plays = Vec::new();
        while let Ok((ts, ev)) = self.rx.try_recv() {
            plays.extend(self.scrobbler.handle(ts, &ev));
        }
        plays.extend(self.scrobbler.flush(now));
        self.append(vault, &plays, "flush final");
    }

    fn current(&self, _now: DateTime<Local>) -> Option<ActivityEvent> {
        None
    }
}

// ── the platform listener ───────────────────────────────────────────────────

/// A decoded notification, stamped with its delivery time.
pub type TimedPlayerEvent = (DateTime<Local>, PlayerEvent);

/// Where the notification callback forwards events while a listener is live.
static ACTIVE: Mutex<Option<Sender<TimedPlayerEvent>>> = Mutex::new(None);

/// Guard for an active listening session. Dropping (or [`MusicListener::stop`])
/// detaches the channel; the observer registration is process-wide.
pub struct MusicListener(());

impl MusicListener {
    pub fn start() -> (MusicListener, Receiver<TimedPlayerEvent>) {
        let (tx, rx) = channel();
        *ACTIVE.lock().unwrap() = Some(tx);
        imp::ensure_registered();
        (MusicListener(()), rx)
    }

    pub fn stop(self) {}
}

impl Drop for MusicListener {
    fn drop(&mut self) {
        *ACTIVE.lock().unwrap() = None;
    }
}

/// Run the calling thread's CFRunLoop until `stopped()` returns true,
/// checking twice a second. Call on the MAIN thread.
pub fn pump_main_run_loop(stopped: impl Fn() -> bool) {
    while !stopped() {
        imp::run_loop_slice(0.5);
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{PlayerEvent, PlayerState, TrackInfo, ACTIVE};
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::sync::Once;

    /// Music also posts `com.apple.iTunes.playerInfo` with an identical
    /// payload — observing both would double every event.
    const NOTIFICATION: &str = "com.apple.Music.playerInfo";

    type CFNotificationCenterRef = *mut c_void;
    type NotificationCallback =
        extern "C" fn(CFNotificationCenterRef, *mut c_void, CFStringRef, *const c_void, CFDictionaryRef);

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
        fn CFNotificationCenterAddObserver(
            center: CFNotificationCenterRef,
            observer: *const c_void,
            call_back: NotificationCallback,
            name: CFStringRef,
            object: *const c_void,
            suspension_behavior: isize,
        );
        fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source_handled: u8) -> i32;
        static kCFRunLoopDefaultMode: CFStringRef;
    }

    const DELIVER_IMMEDIATELY: isize = 4; // kCFNotificationSuspensionBehaviorDeliverImmediately

    static REGISTER: Once = Once::new();

    pub fn ensure_registered() {
        REGISTER.call_once(|| unsafe {
            let center = CFNotificationCenterGetDistributedCenter();
            let name = CFString::new(NOTIFICATION);
            CFNotificationCenterAddObserver(
                center,
                std::ptr::null(),
                on_notification,
                name.as_concrete_TypeRef(),
                std::ptr::null(),
                DELIVER_IMMEDIATELY,
            );
        });
    }

    pub fn run_loop_slice(seconds: f64) {
        // Drain an autorelease pool around every slice — see the module docs
        // for the leak this prevents in a headless host.
        objc2::rc::autoreleasepool(|_| unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds, 0);
        });
    }

    extern "C" fn on_notification(
        _center: CFNotificationCenterRef,
        _observer: *mut c_void,
        _name: CFStringRef,
        _object: *const c_void,
        user_info: CFDictionaryRef,
    ) {
        if let Some(tx) = ACTIVE.lock().unwrap().as_ref() {
            let _ = tx.send((chrono::Local::now(), decode(user_info)));
        }
    }

    fn decode(user_info: CFDictionaryRef) -> PlayerEvent {
        if user_info.is_null() {
            return PlayerEvent { state: PlayerState::Stopped, track: None };
        }
        let dict = unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(user_info) };
        let state = match dict_string(&dict, "Player State").as_str() {
            "Playing" => PlayerState::Playing,
            "Paused" => PlayerState::Paused,
            _ => PlayerState::Stopped,
        };
        let name = dict_string(&dict, "Name");
        let artist = dict_string(&dict, "Artist");
        let track = (!name.is_empty() || !artist.is_empty()).then(|| TrackInfo {
            name,
            artist,
            album: dict_string(&dict, "Album"),
            genre: dict_string(&dict, "Genre"),
            duration_secs: dict_i64(&dict, "Total Time").map(|ms| ms as f64 / 1000.0),
            persistent_id: dict_i64(&dict, "PersistentID")
                .map(|id| format!("{:016X}", id as u64))
                .unwrap_or_default(),
        });
        PlayerEvent { state, track }
    }

    fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> String {
        match dict.find(&CFString::new(key)) {
            Some(v) => v.downcast::<CFString>().map(|s| s.to_string()).unwrap_or_default(),
            None => String::new(),
        }
    }

    fn dict_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
        dict.find(&CFString::new(key))?.downcast::<CFNumber>()?.to_i64()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn ensure_registered() {}

    pub fn run_loop_slice(seconds: f64) {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
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

    fn track(name: &str, duration: Option<f64>) -> TrackInfo {
        TrackInfo {
            name: name.into(),
            artist: "Artist".into(),
            album: "Album".into(),
            genre: "Pop".into(),
            duration_secs: duration,
            persistent_id: format!("{:016X}", name.len() as u64),
        }
    }

    fn playing(t: &TrackInfo) -> PlayerEvent {
        PlayerEvent { state: PlayerState::Playing, track: Some(t.clone()) }
    }

    fn paused(t: &TrackInfo) -> PlayerEvent {
        PlayerEvent { state: PlayerState::Paused, track: Some(t.clone()) }
    }

    fn stopped() -> PlayerEvent {
        PlayerEvent { state: PlayerState::Stopped, track: None }
    }

    fn scrobbler() -> Scrobbler {
        Scrobbler::new(ScrobbleConfig::default())
    }

    /// Byte-parity with Trove's music reader.
    #[test]
    fn append_writes_byte_identical_jsonl() {
        let v = temp_vault("music-parity");
        let play = |start: &str, end: &str| Play {
            start: start.into(),
            end: end.into(),
            seconds_played: 200,
            track: "Take On Me".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            genre: "Pop".into(),
            duration_secs: Some(200.0),
            persistent_id: "000000000000000B".into(),
            full_play: true,
        };
        let p1 = play("2026-06-10T09:00:00-07:00", "2026-06-10T09:03:20-07:00");
        v.append_music_plays(&[p1.clone()]).unwrap();
        v.append_music_plays(&[p1]).unwrap();
        let line = "{\"start\":\"2026-06-10T09:00:00-07:00\",\"end\":\"2026-06-10T09:03:20-07:00\",\
                    \"seconds_played\":200,\"track\":\"Take On Me\",\"artist\":\"Artist\",\"album\":\"Album\",\
                    \"genre\":\"Pop\",\"duration_secs\":200.0,\"persistent_id\":\"000000000000000B\",\"full_play\":true}";
        assert_eq!(
            std::fs::read_to_string(v.root().join("music/plays/2026-06-10.jsonl")).unwrap(),
            format!("{line}\n{line}\n")
        );
    }

    #[test]
    fn full_play_is_recorded_and_flagged() {
        let mut s = scrobbler();
        let t = track("Take On Me", Some(200.0));
        assert!(s.handle(at(9, 0, 0), &playing(&t)).is_none());
        let play = s.handle(at(9, 3, 20), &stopped()).expect("recorded");
        assert_eq!(play.seconds_played, 200);
        assert!(play.full_play);
    }

    #[test]
    fn skip_is_recorded_as_not_full() {
        let mut s = scrobbler();
        let a = track("Skipped", Some(200.0));
        let b = track("Kept", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&a));
        let skip = s.handle(at(9, 0, 20), &stopped()).expect("skips are data");
        assert!(!skip.full_play);
        assert!(s.handle(at(9, 0, 20), &playing(&b)).is_none());
        let play = s.flush(at(9, 2, 0)).expect("100s of 200s is full");
        assert!(play.full_play);
    }

    #[test]
    fn flicker_below_floor_is_dropped() {
        let mut s = scrobbler();
        let t = track("Flicked Past", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&t));
        assert!(s.handle(at(9, 0, 3), &stopped()).is_none());
    }

    #[test]
    fn pause_resume_accumulates_play_time_only() {
        let mut s = scrobbler();
        let t = track("Paused Song", Some(200.0));
        s.handle(at(9, 0, 0), &playing(&t));
        assert!(s.handle(at(9, 1, 0), &paused(&t)).is_none());
        s.handle(at(9, 10, 0), &playing(&t));
        let play = s.handle(at(9, 10, 40), &stopped()).expect("recorded");
        assert_eq!(play.seconds_played, 100);
        assert_eq!(play.end, at(9, 10, 40).to_rfc3339());
    }

    #[test]
    fn unknown_duration_full_at_thirty_seconds() {
        let mut s = scrobbler();
        let t = track("Radio Stream", None);
        s.handle(at(9, 0, 0), &playing(&t));
        assert!(s.handle(at(9, 0, 35), &stopped()).unwrap().full_play);
    }
}
