//! Running at login (`flicks service install|uninstall`; FLK-16): a per-user launchd agent that
//! runs `flicks serve` at login and keeps it running.

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use crate::Error;

/// The agent's launchd label, and the name of its plist.
const LABEL: &str = "com.flicks.server";

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Run `flicks serve` at login and keep it running (macOS).
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Debug, clap::Subcommand)]
pub enum ServiceCommand {
    /// Install the launchd agent and start it now.
    Install,
    /// Stop the agent and remove it.
    Uninstall,
}

pub async fn run(command: Command) -> Result<(), Error> {
    if !cfg!(target_os = "macos") {
        return Err(Error::Usage("`flicks service` isn't supported on this platform yet (macOS only)"));
    }
    let home = crate::home_dir().ok_or(Error::NoHome)?;
    match command {
        Command::Service(ServiceCommand::Install) => install(&home, LABEL, &std::env::current_exe()?),
        Command::Service(ServiceCommand::Uninstall) => uninstall(&home, LABEL),
    }
}

fn plist_path(home: &Path, label: &str) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{label}.plist"))
}

fn log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/flicks.log")
}

fn install(home: &Path, label: &str, exe: &Path) -> Result<(), Error> {
    let plist = plist_path(home, label);
    let log = log_path(home);
    for file in [&plist, &log] {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
    }
    // Replace a loaded agent (an update, or a moved binary) instead of failing on it.
    bootout(label)?;
    std::fs::write(&plist, plist_xml(label, exe, &log))?;
    launchctl(&["bootstrap", &domain()?, &plist.to_string_lossy()])?;
    println!("Installed {}. `flicks serve` now runs at login; its log is {}.", plist.display(), log.display());
    println!("Restart it with: launchctl kickstart -k {}/{label}", domain()?);
    Ok(())
}

fn uninstall(home: &Path, label: &str) -> Result<(), Error> {
    bootout(label)?;
    let mut removed = false;
    for file in [plist_path(home, label), log_path(home)] {
        match std::fs::remove_file(&file) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    println!("{}", if removed { "Uninstalled." } else { "Not installed." });
    Ok(())
}

/// Stops and unloads the agent. It's fine if it isn't loaded.
fn bootout(label: &str) -> Result<(), Error> {
    let target = format!("{}/{label}", domain()?);
    let out = Process::new("launchctl").args(["bootout", &target]).output()?;
    // 3 "No such process" or 113 "Could not find service": it isn't loaded.
    if out.status.success() || matches!(out.status.code(), Some(3 | 113)) {
        return Ok(());
    }
    Err(launchctl_error("bootout", &out))
}

fn launchctl(args: &[&str]) -> Result<(), Error> {
    let out = Process::new("launchctl").args(args).output()?;
    if out.status.success() { Ok(()) } else { Err(launchctl_error(args.first().copied().unwrap_or(""), &out)) }
}

fn launchctl_error(verb: &str, out: &std::process::Output) -> Error {
    let stderr = String::from_utf8_lossy(&out.stderr);
    Error::Io(std::io::Error::other(format!("launchctl {verb} failed ({}): {}", out.status, stderr.trim())))
}

/// The user's GUI launchd domain, `gui/<uid>`.
fn domain() -> Result<String, Error> {
    let out = Process::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&out.stdout);
    let uid = uid.trim();
    if !out.status.success() || uid.is_empty() {
        return Err(Error::Io(std::io::Error::other("couldn't read the user id")));
    }
    Ok(format!("gui/{uid}"))
}

fn plist_xml(label: &str, exe: &Path, log: &Path) -> String {
    let (label, exe, log) = (xml(label), xml(&exe.to_string_lossy()), xml(&log.to_string_lossy()));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Drives the real launchd with a throwaway label, a stub binary and a temp HOME:
/// `cargo test -p flicks-server --lib service -- --ignored` (macOS only; it loads an agent
/// into your GUI session, then removes it).
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn lines(path: &Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |s| s.lines().count())
    }

    #[test]
    #[ignore = "loads a launchd agent into the logged-in GUI session"]
    fn install_restart_uninstall() {
        let label = format!("com.flicks.test.{}", std::process::id());
        let dir = std::env::temp_dir().join(&label);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // Stands in for `flicks serve`: records each start, then stays alive.
        let exe = dir.join("stub.sh");
        let starts = dir.join("starts");
        std::fs::write(&exe, format!("#!/bin/sh\necho \"$1\" >> '{}'\nexec sleep 600\n", starts.display())).unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

        install(&home, &label, &exe).unwrap();
        wait_for("the first start", || lines(&starts) == 1);
        assert_eq!(std::fs::read_to_string(&starts).unwrap(), "serve\n");

        let target = format!("{}/{label}", domain().unwrap());
        launchctl(&["kickstart", "-k", &target]).unwrap();
        wait_for("the restart", || lines(&starts) == 2);

        // Installing again replaces the loaded agent.
        install(&home, &label, &exe).unwrap();
        wait_for("the reinstall start", || lines(&starts) == 3);

        uninstall(&home, &label).unwrap();
        assert!(launchctl(&["print", &target]).is_err(), "agent still loaded");
        assert!(!plist_path(&home, &label).exists());
        assert!(!log_path(&home).exists());
        uninstall(&home, &label).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
