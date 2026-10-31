//! Running at login (`flicks service install|uninstall`): a per-user launchd agent on macOS
//! (FLK-16), systemd user service on Linux (FLK-39) or Scheduled Task on Windows (FLK-52) that
//! runs `flicks serve` and keeps it running.

use std::path::{Path, PathBuf};
use std::process::Command as Process;

use crate::Error;

#[path = "service_windows.rs"]
mod windows;

/// The agent's launchd label, and the name of its plist.
const LABEL: &str = "com.flicks.server";

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Run `flicks serve` at login (at boot on Linux and Windows) and keep it running.
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Debug, clap::Subcommand)]
pub enum ServiceCommand {
    /// Install the launchd agent, systemd user service or Scheduled Task and start it now.
    Install,
    /// Stop the service and remove it.
    Uninstall,
}

pub async fn run(command: Command) -> Result<(), Error> {
    if cfg!(windows) {
        return match command {
            Command::Service(ServiceCommand::Install) => windows::install(&std::env::current_exe()?),
            Command::Service(ServiceCommand::Uninstall) => windows::uninstall(),
        };
    }
    let linux = cfg!(target_os = "linux");
    if !cfg!(target_os = "macos") && !linux {
        return Err(Error::Usage("`flicks service` isn't supported on this platform (macOS, Linux and Windows only)"));
    }
    let home = crate::home_dir().ok_or(Error::NoHome)?;
    match (command, linux) {
        (Command::Service(ServiceCommand::Install), false) => install(&home, LABEL, &std::env::current_exe()?),
        (Command::Service(ServiceCommand::Uninstall), false) => uninstall(&home, LABEL),
        (Command::Service(ServiceCommand::Install), true) => {
            install_systemd(&unit_path(&home, config_home().as_deref()), &std::env::current_exe()?)
        }
        (Command::Service(ServiceCommand::Uninstall), true) => {
            uninstall_systemd(&unit_path(&home, config_home().as_deref()))
        }
    }
}

/// `$XDG_CONFIG_HOME`, if set to an absolute path (the spec says to ignore a relative one).
fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute())
}

/// The systemd user unit's name.
const UNIT: &str = "flicks.service";

/// Where systemd looks for user units: `$XDG_CONFIG_HOME/systemd/user`, else `~/.config/...`.
fn unit_path(home: &Path, config_home: Option<&Path>) -> PathBuf {
    config_home.map_or_else(|| home.join(".config"), Path::to_path_buf).join("systemd/user").join(UNIT)
}

fn install_systemd(unit: &Path, exe: &Path) -> Result<(), Error> {
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(unit, unit_file(exe))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT])?;
    // Restart rather than `enable --now`, so a reinstall (an update, or a moved binary) takes effect.
    systemctl(&["restart", UNIT])?;
    println!("Installed {}. `flicks serve` is running; its log is `journalctl --user -u flicks`.", unit.display());
    // Lingering starts the user's services at boot instead of at their first login.
    let user = user_name()?;
    match Process::new("loginctl").args(["enable-linger", &user]).output() {
        Ok(out) if out.status.success() => {
            println!("Lingering is on, so it starts at boot. Restart it with: systemctl --user restart {UNIT}");
        }
        _ => println!("Couldn't enable lingering; run `sudo loginctl enable-linger {user}` so it starts at boot."),
    }
    Ok(())
}

fn uninstall_systemd(unit: &Path) -> Result<(), Error> {
    if !unit.exists() {
        println!("Not installed.");
        return Ok(());
    }
    // Stops it and removes the enable link; fine if it was never enabled.
    let _ = systemctl(&["disable", "--now", UNIT]);
    std::fs::remove_file(unit)?;
    systemctl(&["daemon-reload"])?;
    // Lingering stays as it is: it's account-wide, and other user services may rely on it.
    println!("Uninstalled.");
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<(), Error> {
    let out = Process::new("systemctl").arg("--user").args(args).output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(Error::Io(std::io::Error::other(format!(
        "systemctl --user {} failed ({}): {}",
        args.first().copied().unwrap_or(""),
        out.status,
        stderr.trim()
    ))))
}

fn user_name() -> Result<String, Error> {
    let out = Process::new("id").arg("-un").output()?;
    let name = String::from_utf8_lossy(&out.stdout);
    let name = name.trim();
    if !out.status.success() || name.is_empty() {
        return Err(Error::Io(std::io::Error::other("couldn't read the user name")));
    }
    Ok(name.to_owned())
}

fn unit_file(exe: &Path) -> String {
    // Quoted, with the characters systemd expands in a command line escaped.
    let exe = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    format!(
        "[Unit]\nDescription=Flicks media server\n\n[Service]\nExecStart=\"{exe}\" serve\nRestart=on-failure\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n"
    )
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

#[cfg(test)]
mod systemd_tests {
    use super::*;

    #[test]
    fn unit_path_honours_xdg_config_home() {
        let home = Path::new("/home/ana");
        assert_eq!(unit_path(home, None), Path::new("/home/ana/.config/systemd/user/flicks.service"));
        let config = Path::new("/srv/config");
        assert_eq!(unit_path(home, Some(config)), Path::new("/srv/config/systemd/user/flicks.service"));
    }

    #[test]
    fn unit_restarts_after_a_pause() {
        let unit = unit_file(Path::new("/opt/flicks 1/50%/flicks"));
        assert!(unit.contains("ExecStart=\"/opt/flicks 1/50%%/flicks\" serve\n"), "{unit}");
        assert!(unit.contains("Restart=on-failure\nRestartSec=2\n"), "{unit}");
    }
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
