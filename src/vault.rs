//! The collector's view of a Trove vault: just enough of the vault spec to
//! write its streams correctly, and nothing that belongs to the app.
//!
//! Everything here follows `docs/vault-spec/conventions.md` in the Trove
//! repo: RFC3339 local timestamps, one file per local day, one JSON object
//! per line, append-only, atomic (tmp + rename) for anything rewritten. The
//! collector is deliberately *not* linked against Trove's core crate — it is
//! the reference proof that any program can write to a vault by following
//! the spec.
//!
//! Vault-relative paths are resolved through [`Vault::resolve`], which
//! refuses absolute paths and `..` so a bug can never write outside the root.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::activity::ActivityEvent;
use crate::browser::{LiveSpan, LiveState};

/// Hub toggles, shared with the Trove app (`.trove/integrations.json`).
const SETTINGS_FILE: &str = ".trove/integrations.json";
/// Heartbeat the app reads to show "collector running / not running".
const STATE_FILE: &str = ".trove/watcher-state.json";
/// Single-writer lock: only one collector process per vault.
const LOCK_FILE: &str = ".trove/watcher.lock";
/// Ephemeral "watching now" sidecars for the browser hosts.
const LIVE_DIR: &str = ".trove/live";

/// The integrations this collector implements, with their default toggle
/// state. Ids match the cards in the Trove app's hub, which writes the
/// settings file; `default_on: false` entries need an explicit opt-in.
pub const INTEGRATIONS: &[(&str, bool)] = &[
    ("activity", true),
    ("music-scrobbler", true),
    ("browser-extension", true),
    ("browser-ads", true),
    ("browser-ads-identify", false),
];

/// A vault root plus the handful of operations a collector needs.
#[derive(Debug, Clone)]
pub struct Vault {
    root: PathBuf,
}

/// The app's toggle file: `disabled` opts out of default-on integrations,
/// `enabled` opts in to the default-off ones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegrationSettings {
    #[serde(default)]
    pub disabled: BTreeSet<String>,
    #[serde(default)]
    pub enabled: BTreeSet<String>,
}

/// Is `id` on under these settings? Unknown ids are off.
pub fn enabled_in(settings: &IntegrationSettings, id: &str) -> bool {
    match INTEGRATIONS.iter().find(|(i, _)| *i == id) {
        Some((_, true)) => !settings.disabled.contains(id),
        Some((_, false)) => settings.enabled.contains(id),
        None => false,
    }
}

/// Heartbeat written every poll while collecting. The Trove app reads it
/// (and checks freshness) to show collector status and the in-progress
/// activity event. Removed on graceful shutdown; stale after a crash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub pid: u32,
    /// Always "trove-collector" for this binary.
    pub role: String,
    /// RFC3339 local time of the last tick.
    pub updated: String,
    /// The in-progress (not yet written) activity event, if any.
    pub current: Option<ActivityEvent>,
    /// Resident memory of the collector process, for the memory audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_mb: Option<u64>,
}

impl Vault {
    /// Default vault location: ~/Documents/Trove.
    pub fn default_root() -> PathBuf {
        dirs::home_dir().expect("no home directory").join("Documents").join("Trove")
    }

    /// Open the vault at `root`, creating the root and `.trove/` if missing.
    /// The collector creates its own stream folders lazily on first append.
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join(".trove"))
            .with_context(|| format!("creating vault at {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a vault-relative path, refusing anything that escapes the root.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let p = Path::new(rel);
        if p.is_absolute()
            || p.components().any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("path escapes vault: {rel}");
        }
        Ok(self.root.join(p))
    }

    /// Append records to `<dir>/YYYY-MM-DD.jsonl`, grouped by the day prefix
    /// of `ts(record)`. One JSON object per line, newline-terminated; parent
    /// directories created on demand. With `flock`, an exclusive lock is
    /// held per file while writing — for streams with more than one
    /// legitimate writer process (each Chrome profile runs its own host).
    ///
    /// A record whose timestamp has no `YYYY-MM-DD` prefix is an error and
    /// fails the whole call before any file is touched.
    pub fn append_day_jsonl<T: Serialize>(
        &self,
        dir: &str,
        records: &[T],
        ts: impl Fn(&T) -> &str,
        flock: bool,
    ) -> Result<()> {
        let mut by_day: BTreeMap<&str, Vec<&T>> = BTreeMap::new();
        for r in records {
            let t = ts(r);
            let key = day_key(t).with_context(|| {
                format!("cannot partition record into {dir}/: timestamp {t:?} has no date prefix")
            })?;
            by_day.entry(key).or_default().push(r);
        }
        for (day, rs) in by_day {
            let rel = format!("{dir}/{day}.jsonl");
            let path = self.resolve(&rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {rel}"))?;
            #[cfg(unix)]
            if flock {
                use std::os::unix::io::AsRawFd;
                let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
                if rc != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("locking {rel}"));
                }
            }
            for r in rs {
                writeln!(f, "{}", serde_json::to_string(r)?)?;
            }
            // Dropping `f` releases the flock.
        }
        Ok(())
    }

    /// Every record of one day file, in file order (tests).
    #[cfg(test)]
    /// Missing file = empty; unparseable lines are skipped.
    pub fn read_day_jsonl<T: DeserializeOwned>(&self, dir: &str, day: &str) -> Result<Vec<T>> {
        let rel = format!("{dir}/{day}.jsonl");
        let path = self.resolve(&rel)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<T>(l).ok())
            .collect())
    }

    // ── hub toggles ────────────────────────────────────────────────────────

    /// The app's toggle file; missing or unreadable means all defaults.
    pub fn integration_settings(&self) -> IntegrationSettings {
        let Ok(path) = self.resolve(SETTINGS_FILE) else {
            return IntegrationSettings::default();
        };
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Is the integration on right now? Re-reads the (tiny) settings file so
    /// a toggle in the app takes effect within one poll.
    pub fn integration_enabled(&self, id: &str) -> bool {
        enabled_in(&self.integration_settings(), id)
    }

    // ── single-writer lock ─────────────────────────────────────────────────

    /// Try to take the vault's collector lock. The returned `File` *is* the
    /// lock: keep it alive while collecting; the kernel releases it when the
    /// process exits, crash included.
    #[cfg(unix)]
    pub fn try_lock(&self) -> Result<Option<File>> {
        use std::os::unix::io::AsRawFd;
        let path = self.resolve(LOCK_FILE)?;
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .context("opening watcher.lock")?;
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        Ok((rc == 0).then_some(f))
    }

    #[cfg(not(unix))]
    pub fn try_lock(&self) -> Result<Option<File>> {
        let path = self.resolve(LOCK_FILE)?;
        Ok(Some(OpenOptions::new().create(true).write(true).open(&path)?))
    }

    // ── heartbeat ──────────────────────────────────────────────────────────

    /// Atomic write (temp + rename) so the app never reads a torn file.
    pub fn write_heartbeat(&self, state: &Heartbeat) -> Result<()> {
        let path = self.resolve(STATE_FILE)?;
        write_atomic(&path, &serde_json::to_vec(state)?)
    }

    pub fn read_heartbeat(&self) -> Option<Heartbeat> {
        let path = self.resolve(STATE_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    pub fn clear_heartbeat(&self) -> Result<()> {
        let path = self.resolve(STATE_FILE)?;
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    // ── browser live sidecars ──────────────────────────────────────────────

    /// Replace this host's "watching now" sidecar with its open spans. `key`
    /// identifies the writer (the host pid) so concurrent Chrome profiles
    /// don't clobber each other. Ephemeral runtime state, not vault data.
    pub fn write_browser_live(&self, browser: &str, key: &str, spans: &[LiveSpan]) -> Result<()> {
        let dir = self.resolve(LIVE_DIR)?;
        fs::create_dir_all(&dir).context("creating live dir")?;
        let state = LiveState {
            browser: browser.to_string(),
            updated: now_epoch(),
            spans: spans.to_vec(),
        };
        write_atomic(&dir.join(format!("browser-{key}.json")), &serde_json::to_vec(&state)?)
            .with_context(|| format!("publishing live sidecar {key}"))
    }

    /// Remove this host's sidecar on disconnect. Absent file is success.
    pub fn clear_browser_live(&self, key: &str) -> Result<()> {
        let path = self.resolve(LIVE_DIR)?.join(format!("browser-{key}.json"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("clearing live sidecar {key}")),
        }
    }
}

/// The `YYYY-MM-DD` prefix of an RFC3339 timestamp, if it has one.
fn day_key(ts: &str) -> Option<&str> {
    let key = ts.get(..10)?;
    key.bytes()
        .enumerate()
        .all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() })
        .then_some(key)
}

/// Write a whole file atomically: sibling temp file, then rename over the
/// target, so a reader never sees a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".into(),
    });
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(())
}

pub fn now_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn temp_vault(name: &str) -> Vault {
    let dir = std::env::temp_dir().join(format!("trove-collector-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    Vault::open(dir).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Rec {
        ts: String,
        n: u32,
    }

    #[test]
    fn append_partitions_by_day_and_is_byte_exact() {
        let v = temp_vault("append");
        let recs = vec![
            Rec { ts: "2026-06-10T09:00:00-07:00".into(), n: 1 },
            Rec { ts: "2026-06-11T09:00:00-07:00".into(), n: 2 },
            Rec { ts: "2026-06-10T10:00:00-07:00".into(), n: 3 },
        ];
        v.append_day_jsonl("stream", &recs, |r| &r.ts, false).unwrap();
        v.append_day_jsonl("stream", &recs[..1], |r| &r.ts, true).unwrap();
        let d10 = fs::read_to_string(v.root().join("stream/2026-06-10.jsonl")).unwrap();
        assert_eq!(
            d10,
            "{\"ts\":\"2026-06-10T09:00:00-07:00\",\"n\":1}\n\
             {\"ts\":\"2026-06-10T10:00:00-07:00\",\"n\":3}\n\
             {\"ts\":\"2026-06-10T09:00:00-07:00\",\"n\":1}\n"
        );
        let back: Vec<Rec> = v.read_day_jsonl("stream", "2026-06-11").unwrap();
        assert_eq!(back, vec![Rec { ts: "2026-06-11T09:00:00-07:00".into(), n: 2 }]);
    }

    #[test]
    fn malformed_timestamp_writes_nothing() {
        let v = temp_vault("malformed");
        let recs = vec![
            Rec { ts: "2026-06-10T09:00:00-07:00".into(), n: 1 },
            Rec { ts: "not a date".into(), n: 2 },
        ];
        assert!(v.append_day_jsonl("stream", &recs, |r| &r.ts, false).is_err());
        assert!(!v.root().join("stream").exists());
    }

    #[test]
    fn resolve_rejects_escapes() {
        let v = temp_vault("resolve");
        assert!(v.resolve("../x").is_err());
        assert!(v.resolve("/etc/passwd").is_err());
        assert!(v.resolve("activity/2026-06-10.jsonl").is_ok());
    }

    #[test]
    fn toggles_follow_defaults_and_settings() {
        let v = temp_vault("toggles");
        assert!(v.integration_enabled("activity"));
        assert!(!v.integration_enabled("browser-ads-identify"), "default off");
        assert!(!v.integration_enabled("not-a-thing"));
        fs::write(
            v.root().join(SETTINGS_FILE),
            r#"{"disabled":["activity"],"enabled":["browser-ads-identify"]}"#,
        )
        .unwrap();
        assert!(!v.integration_enabled("activity"));
        assert!(v.integration_enabled("browser-ads-identify"));
        assert!(v.integration_enabled("music-scrobbler"));
    }

    #[test]
    #[cfg(unix)]
    fn lock_is_exclusive_until_released() {
        let v = temp_vault("lock");
        let first = v.try_lock().unwrap();
        assert!(first.is_some());
        assert!(v.try_lock().unwrap().is_none(), "second lock must fail");
        drop(first);
        assert!(v.try_lock().unwrap().is_some());
    }

    #[test]
    fn heartbeat_round_trip_and_clear() {
        let v = temp_vault("heartbeat");
        assert!(v.read_heartbeat().is_none());
        v.write_heartbeat(&Heartbeat {
            pid: 42,
            role: "trove-collector".into(),
            updated: "2026-06-10T09:00:00-07:00".into(),
            current: None,
            rss_mb: Some(31),
        })
        .unwrap();
        let h = v.read_heartbeat().unwrap();
        assert_eq!(h.pid, 42);
        assert_eq!(h.rss_mb, Some(31));
        v.clear_heartbeat().unwrap();
        assert!(v.read_heartbeat().is_none());
    }
}
