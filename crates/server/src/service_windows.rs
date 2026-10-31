//! The Windows half of `flicks service`: a per-user Scheduled Task, driven through `schtasks.exe`.
//!
//! It runs as the current user with an S4U logon (no stored password, no admin), started by a
//! boot trigger, so the server comes back after a reboot without anyone logging on. S4U tasks
//! run in session 0, so there is no console window; the user's `%APPDATA%` and profile paths
//! are set as in a normal logon, which keeps the data dir per-user. Both the registration and
//! boot triggers repeat every minute; `IgnoreNew` skips a running copy, so a server that exits or is
//! killed is back within a minute; the task's own `RestartOnFailure` setting did not restart
//! on a nonzero exit or a kill on Windows 10.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use crate::Error;

/// The task's name.
const TASK: &str = "Flicks Server";

pub(super) fn install(exe: &Path) -> Result<(), Error> {
    let user = user_name()?;
    // schtasks reads UTF-16 task files reliably; the BOM tells it so.
    let mut file = vec![0xFF, 0xFE];
    for unit in task_xml(&user, exe).encode_utf16() {
        file.extend_from_slice(&unit.to_le_bytes());
    }
    let dir = crate::data_dir()?;
    fs::create_dir_all(&dir)?;
    #[cfg(windows)]
    flicks_platform::permissions::owner_only(&dir)?;
    let path = dir.join(format!("task-{}.xml", crate::keys::encode(&crate::keys::random::<16>()?)));
    let mut xml = File::create_new(&path)?;
    let created = (|| {
        xml.write_all(&file)?;
        drop(xml);
        // End the old binary on reinstall; the registration trigger starts the new one.
        let _ = schtasks(&["/End", "/TN", TASK]);
        schtasks(&["/Create", "/TN", TASK, "/XML", &path.to_string_lossy(), "/F"])
    })();
    let removed = fs::remove_file(&path);
    created?;
    removed?;
    println!("Installed the \"{TASK}\" scheduled task. `flicks serve` is running and starts at boot.");
    println!("Restart it with: schtasks /End /TN \"{TASK}\" & schtasks /Run /TN \"{TASK}\"");
    Ok(())
}

pub(super) fn uninstall() -> Result<(), Error> {
    if schtasks(&["/Query", "/TN", TASK]).is_err() {
        println!("Not installed.");
        return Ok(());
    }
    let _ = schtasks(&["/End", "/TN", TASK]);
    schtasks(&["/Delete", "/TN", TASK, "/F"])?;
    println!("Uninstalled.");
    Ok(())
}

fn system_tool(name: &str) -> io::Result<Process> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::other("missing absolute SystemRoot"))?;
    Ok(Process::new(root.join("System32").join(name)))
}

/// `DOMAIN\user` from `whoami`; `%USERDOMAIN%` is wrong under SSH (it says WORKGROUP).
fn user_name() -> Result<String, Error> {
    let out = system_tool("whoami.exe")?.output()?;
    let name = String::from_utf8_lossy(&out.stdout);
    let name = name.trim();
    if !out.status.success() || name.is_empty() {
        return Err(Error::Io(std::io::Error::other("couldn't read the user name")));
    }
    Ok(name.to_owned())
}

fn schtasks(args: &[&str]) -> Result<(), Error> {
    let out = system_tool("schtasks.exe")?.args(args).output()?;
    if out.status.success() {
        return Ok(());
    }
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Err(Error::Io(std::io::Error::other(format!(
        "schtasks {} failed ({}): {}",
        args.first().copied().unwrap_or(""),
        out.status,
        text.trim()
    ))))
}

fn task_xml(user: &str, exe: &Path) -> String {
    let (user, exe) = (super::xml(user), super::xml(&exe.to_string_lossy()));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <RegistrationTrigger>
      <Enabled>true</Enabled>
      <Repetition><Interval>PT1M</Interval></Repetition>
    </RegistrationTrigger>
    <BootTrigger>
      <Enabled>true</Enabled>
      <Repetition><Interval>PT1M</Interval></Repetition>
    </BootTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>S4U</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Hidden>true</Hidden>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{exe}"</Command>
      <Arguments>serve</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_runs_serve_for_the_user_at_boot_and_repeats() {
        let xml = task_xml("PC\\Ana & Co", Path::new("C:\\Program Files\\Flicks\\flicks.exe"));
        assert!(xml.contains("<UserId>PC\\Ana &amp; Co</UserId>"), "{xml}");
        assert!(xml.contains("<Command>\"C:\\Program Files\\Flicks\\flicks.exe\"</Command>"), "{xml}");
        assert!(xml.contains("<Arguments>serve</Arguments>"), "{xml}");
        assert!(xml.contains("<BootTrigger>") && xml.contains("<LogonType>S4U</LogonType>"), "{xml}");
        assert!(xml.contains("<Repetition><Interval>PT1M</Interval></Repetition>"), "{xml}");
    }
}
