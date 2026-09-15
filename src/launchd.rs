//! launchd management: install/uninstall the user agent, register the Chrome
//! native messaging host manifest, and report status.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::browser::{native_host_manifest_path, EXTENSION_ID, NATIVE_HOST_NAME};
use crate::vault::Vault;

/// launchd label for the collector. Shared with the Trove app's "is the
/// collector installed" check (it looks for the plist by this name).
pub const LABEL: &str = "com.davidwills.trove-collector";

pub fn plist_path() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
        .context("no home directory")
}

fn logs_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home directory")?.join("Library/Logs/trove"))
}

#[cfg(unix)]
fn uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

/// True if launchd currently has our service bootstrapped in the gui domain.
fn service_loaded() -> bool {
    Command::new("launchctl")
        .args(["print", &format!("gui/{}/{}", uid(), LABEL)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// (Re)load the launch agent from `plist`, race-free. `launchctl bootout`
/// returns before teardown finishes, so an immediate `bootstrap` can fail
/// with `5: Input/output error`; poll until the service is gone, then
/// bootstrap with a short retry.
fn restart_service(plist: &Path) -> Result<()> {
    let domain = format!("gui/{}", uid());
    let target = format!("{}/{}", domain, LABEL);

    let _ = Command::new("launchctl").args(["bootout", &target]).output();
    for _ in 0..50 {
        if !service_loaded() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let mut last_err = String::new();
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let out = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(plist)
            .output()
            .context("running launchctl bootstrap")?;
        if out.status.success() {
            return Ok(());
        }
        if service_loaded() {
            let _ = Command::new("launchctl").args(["kickstart", "-k", &target]).output();
            return Ok(());
        }
        last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    }
    bail!("launchctl bootstrap failed after retries: {last_err}");
}

pub fn install() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launchd management is macOS-only");
    }
    let exe = std::env::current_exe()
        .context("locating trove-collector binary")?
        .canonicalize()
        .context("canonicalizing binary path")?;
    let logs = logs_dir()?;
    std::fs::create_dir_all(&logs)?;

    let plist = plist_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Background</string>
    <key>StandardOutPath</key><string>{logs}/trove-collector.log</string>
    <key>StandardErrorPath</key><string>{logs}/trove-collector.err.log</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = exe.display(),
        logs = logs.display(),
    );
    std::fs::write(&plist, body).with_context(|| format!("writing {}", plist.display()))?;
    restart_service(&plist)?;
    println!("trove-collector: installed and started ({})", exe.display());
    println!("trove-collector: logs at {}/trove-collector.log", logs.display());

    let manifest = install_native_host_manifest(&exe)?;
    println!("trove-collector: Chrome native messaging host manifest at {}", manifest.display());
    Ok(())
}

/// Register this binary as the browser extension's native messaging host.
/// Chrome reads the manifest at connect time, so reinstalls take effect on
/// the extension's next reconnect.
fn install_native_host_manifest(exe: &Path) -> Result<PathBuf> {
    let path = native_host_manifest_path().context("no home directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({
        "name": NATIVE_HOST_NAME,
        "description": "Trove browser watcher host (writes tab spans into the local vault)",
        "path": exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&body)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn uninstall() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launchd management is macOS-only");
    }
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid(), LABEL)])
        .output();
    let plist = plist_path()?;
    if plist.exists() {
        std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        println!("trove-collector: stopped and removed {}", plist.display());
    } else {
        println!("trove-collector: not installed (no plist at {})", plist.display());
    }
    if let Some(manifest) = native_host_manifest_path() {
        if manifest.exists() {
            std::fs::remove_file(&manifest)
                .with_context(|| format!("removing {}", manifest.display()))?;
            println!("trove-collector: removed native messaging host manifest");
        }
    }
    Ok(())
}

pub fn status(root: PathBuf) -> Result<()> {
    let plist = plist_path()?;
    println!(
        "launch agent: {}",
        if plist.exists() { format!("installed ({})", plist.display()) } else { "not installed".into() }
    );
    if cfg!(target_os = "macos") {
        println!("launchd service: {}", if service_loaded() { "loaded" } else { "not loaded" });
        println!(
            "screen recording: {}",
            if crate::sampler::screen_recording_ok() { "granted" } else { "not granted (window titles will be empty)" }
        );
    }
    println!(
        "browser extension host: {}",
        match native_host_manifest_path() {
            Some(m) if m.exists() => format!("manifest installed ({})", m.display()),
            _ => "manifest not installed (run `trove-collector install`)".into(),
        }
    );
    let vault = Vault::open(root)?;
    println!("vault: {}", vault.root().display());
    match vault.read_heartbeat() {
        Some(h) if is_fresh(&h.updated) => {
            let doing = h
                .current
                .as_ref()
                .map(|c| if c.afk { "away".to_string() } else { format!("in {}", c.app) })
                .unwrap_or_else(|| "idle".into());
            let mem = h.rss_mb.map(|m| format!(", {m} MB")).unwrap_or_default();
            println!("collector: running (pid {}{mem}), currently {doing}", h.pid);
        }
        Some(_) => println!("collector: none (stale heartbeat — last owner likely crashed)"),
        None => println!("collector: none"),
    }
    let settings = vault.integration_settings();
    for (id, _) in crate::vault::INTEGRATIONS {
        println!(
            "  {id}: {}",
            if crate::vault::enabled_in(&settings, id) { "on" } else { "off" }
        );
    }
    Ok(())
}

/// A heartbeat is live if it was written within a few polls.
pub fn is_fresh(updated: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(updated)
        .map(|t| {
            chrono::Local::now().signed_duration_since(t).num_seconds()
                <= (crate::activity::POLL_SECS * 3) as i64
        })
        .unwrap_or(false)
}
