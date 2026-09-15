//! trove-collector — the always-on collector for a Trove vault.
//!
//! Writes three streams into `~/Documents/Trove` (or `--vault` /
//! `TROVE_VAULT`) under the Trove vault spec, with no dependency on the
//! Trove app or its crates:
//!
//! - `activity/`      what app and window is frontmost, with idle detection
//! - `browser/`       tab engagement spans, via the Chrome extension
//!                    (`browser/ads/` when the extension's ad observer is on)
//! - `music/plays/`   Apple Music plays as they happen
//!
//! Commands:
//!   trove-collector run          collect until SIGTERM/SIGINT (launchd mode; default)
//!   trove-collector install      write + start the launch agent (runs at login)
//!                                and the Chrome native messaging host manifest
//!   trove-collector uninstall    stop and remove the launch agent
//!   trove-collector status       installed? running? memory? which streams are on?
//!   trove-collector permission   prompt for Screen Recording (window titles)
//!
//! Chrome also spawns this binary as the extension's native messaging host,
//! passing the extension origin as the first argument — `main` dispatches
//! that to [`native_host`].

mod activity;
mod ads;
mod browser;
mod launchd;
mod music;
mod native_host;
mod runner;
mod sampler;
mod vault;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};

use runner::Control;
use vault::Vault;

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    let result = match cmd {
        origin if origin.starts_with("chrome-extension://") => {
            vault_root(&[]).and_then(|root| native_host::run(origin, root))
        }
        "run" => run(&args[1..]),
        "install" => launchd::install(),
        "uninstall" => launchd::uninstall(),
        "status" => vault_root(&args[1..]).and_then(launchd::status),
        "permission" => {
            let granted = sampler::request_screen_recording();
            println!(
                "screen recording: {}",
                if granted { "granted" } else { "not granted — System Settings → Privacy & Security → Screen Recording" }
            );
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("trove-collector: unknown command `{other}`\n");
            print_help();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("trove-collector: {e:#}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "trove-collector — always-on collector for a Trove vault\n\n\
         usage: trove-collector [run|install|uninstall|status|permission] [--vault <path>]\n\n\
         run          collect until SIGTERM/SIGINT (default)\n\
         install      write the launch agent and start it (runs at login);\n\
                      also registers the Chrome native messaging host\n\
         uninstall    stop the agent and remove the plist + host manifest\n\
         status       show install/running/memory state and stream toggles\n\
         permission   prompt for Screen Recording (needed for window titles)\n\n\
         The vault is ~/Documents/Trove unless --vault or TROVE_VAULT says otherwise."
    );
}

/// The vault root: --vault flag > TROVE_VAULT env > ~/Documents/Trove.
fn vault_root(args: &[String]) -> Result<PathBuf> {
    if let Some(i) = args.iter().position(|a| a == "--vault") {
        let path = args.get(i + 1).context("--vault requires a path")?;
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("TROVE_VAULT") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    Ok(Vault::default_root())
}

fn run(args: &[String]) -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    let root = vault_root(args)?;
    println!("trove-collector: watching vault at {} (pid {})", root.display(), std::process::id());
    if !sampler::screen_recording_ok() {
        println!(
            "trove-collector: Screen Recording not granted for this binary — other apps' window \
             titles will be empty (app-level tracking works regardless). Run `trove-collector permission`."
        );
    }

    let control = Control::new();
    let bridge = control.clone();
    std::thread::spawn(move || loop {
        if STOP.load(Ordering::SeqCst) {
            bridge.stop();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });

    // The collector loop runs on a worker thread; the MAIN thread pumps the
    // CF run loop, because distributed notifications (the Music scrobbler's
    // source) are only delivered on the process's main run loop.
    let worker = {
        let control = control.clone();
        std::thread::Builder::new()
            .name("trove-collector".into())
            .spawn(move || runner::run(root, control))
            .context("spawning collector thread")?
    };
    music::pump_main_run_loop(|| control.stopped());
    match worker.join() {
        Ok(result) => result?,
        Err(_) => bail!("collector thread panicked"),
    }
    println!("trove-collector: stopped cleanly (open spans flushed)");
    Ok(())
}
