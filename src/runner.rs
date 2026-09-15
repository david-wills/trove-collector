//! The always-on loop: take the vault's single-writer lock, tick every live
//! collector each poll, heartbeat, and log memory.
//!
//! Exactly one collector process may write the live streams at a time.
//! Coordination is an OS advisory file lock (`.trove/watcher.lock`): the
//! holder runs the sample→tick→append loop and heartbeats
//! `.trove/watcher-state.json`; any other instance idles and retries the
//! lock each poll, so when the owner exits (the kernel releases flock on
//! process death, crash included) the next one takes over within a poll.
//!
//! Memory is part of the contract: the heartbeat carries the process's
//! resident size and the log gets a line every [`MEMORY_LOG_SECS`], so a
//! leak shows up in the log long before it shows up in Activity Monitor.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local};

use crate::activity::{ActivityEvent, ActivityLive, POLL_SECS};
use crate::music::MusicLive;
use crate::vault::{Heartbeat, Vault};

/// How often to write a memory line to the log.
const MEMORY_LOG_SECS: u64 = 600;

/// A collector that runs every poll while the process holds the lock.
pub trait LiveCollector: Send {
    /// The hub toggle id this collector honours.
    fn id(&self) -> &'static str;
    /// Called every poll with the toggle state. Each impl owns its disabled
    /// semantics (close out open spans; discard rather than queue events
    /// received while off).
    fn tick(&mut self, vault: &Vault, now: DateTime<Local>, enabled: bool);
    /// Graceful shutdown: close out open spans and write them.
    fn shutdown(&mut self, vault: &Vault, now: DateTime<Local>);
    /// The in-progress activity event, for the heartbeat (activity only).
    fn current(&self, now: DateTime<Local>) -> Option<ActivityEvent>;
}

/// Shared stop flag for the loop.
#[derive(Clone, Default)]
pub struct Control {
    stop: Arc<AtomicBool>,
}

impl Control {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// Blocking: contend for the lock and, while holding it, run the collector
/// loop. Returns only when [`Control::stop`] is called (open spans are
/// flushed first). Collection errors inside the loop are logged and skipped,
/// never fatal — a daemon must outlive transient failures.
pub fn run(root: PathBuf, control: Control) -> Result<()> {
    let vault = Vault::open(root)?;
    while !control.stopped() {
        match vault.try_lock()? {
            Some(lock) => {
                owner_loop(&vault, &control);
                drop(lock);
            }
            None => {
                eprintln!("trove-collector: another collector holds the vault lock; waiting");
                sleep_unless_stopped(&control, Duration::from_secs(POLL_SECS));
            }
        }
    }
    Ok(())
}

fn owner_loop(vault: &Vault, control: &Control) {
    let mut live: Vec<Box<dyn LiveCollector>> =
        vec![Box::new(ActivityLive::new()), Box::new(MusicLive::new())];
    let mut last_memory_log = Instant::now();
    log_memory();
    while !control.stopped() {
        sleep_unless_stopped(control, Duration::from_secs(POLL_SECS));
        if control.stopped() {
            break;
        }
        // Hub toggles are re-read every tick — one tiny file — so a switch
        // in the app takes effect within a poll, no restart.
        let settings = vault.integration_settings();
        let now = Local::now();
        for c in &mut live {
            let on = crate::vault::enabled_in(&settings, c.id());
            c.tick(vault, now, on);
        }
        let current = live.iter().find_map(|c| c.current(now));
        let state = Heartbeat {
            pid: std::process::id(),
            role: "trove-collector".into(),
            updated: now.to_rfc3339(),
            current,
            rss_mb: rss_bytes().map(|b| b / (1024 * 1024)),
        };
        if let Err(e) = vault.write_heartbeat(&state) {
            eprintln!("trove-collector: failed to write heartbeat: {e:#}");
        }
        if last_memory_log.elapsed().as_secs() >= MEMORY_LOG_SECS {
            log_memory();
            last_memory_log = Instant::now();
        }
    }
    let now = Local::now();
    for c in &mut live {
        c.shutdown(vault, now);
    }
    if let Err(e) = vault.clear_heartbeat() {
        eprintln!("trove-collector: failed to clear heartbeat: {e:#}");
    }
}

fn log_memory() {
    match rss_bytes() {
        Some(b) => eprintln!("trove-collector: memory rss={} MB", b / (1024 * 1024)),
        None => eprintln!("trove-collector: memory rss=unknown"),
    }
}

/// Sleep `total` in short slices, returning early once stop is requested.
fn sleep_unless_stopped(control: &Control, total: Duration) {
    let slice = Duration::from_millis(250);
    let mut elapsed = Duration::ZERO;
    while elapsed < total && !control.stopped() {
        std::thread::sleep(slice);
        elapsed += slice;
    }
}

/// Resident set size of this process, in bytes.
#[cfg(target_os = "macos")]
pub fn rss_bytes() -> Option<u64> {
    use std::mem::MaybeUninit;
    let mut info = MaybeUninit::<libc::proc_taskinfo>::uninit();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            std::process::id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };
    if rc != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(info.pti_resident_size)
}

#[cfg(target_os = "linux")]
pub fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_is_reported_on_supported_platforms() {
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            let rss = rss_bytes().expect("rss available");
            assert!(rss > 0);
        }
    }
}
