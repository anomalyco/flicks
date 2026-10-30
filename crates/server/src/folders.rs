//! Which folders may be added: never one that is, is inside, or holds a system or secret location,
//! nor the home folder or one that holds it (docs/architecture.md, "Folders"), judged after
//! resolving symlinks. Folders inside the OS temp folder are fine even where that is under `/var`
//! (macOS).

use std::path::{Path, PathBuf};

/// System locations, refused on every platform where they exist.
const SYSTEM: &[&str] =
    &["/etc", "/var", "/usr", "/bin", "/sbin", "/System", "/Library", "/dev", "/proc", "/sys", "/boot", "/root"];

/// Secret locations under the home directory.
const SECRET: &[&str] = &[".ssh", ".gnupg", ".aws", ".config", ".kube", ".docker", "Library"];

/// Resolves `path` and checks it may be added; the error says why not.
pub fn check(path: &Path, data_dir: &Path) -> Result<PathBuf, String> {
    let resolved = path.canonicalize().map_err(|e| format!("can't open {}: {e}", path.display()))?;
    if !resolved.is_dir() {
        return Err(format!("{} is not a folder", resolved.display()));
    }
    let home = crate::home_dir().map(|home| home.canonicalize().unwrap_or(home));
    if let Some(home) = &home
        && home.starts_with(&resolved)
    {
        return Err(format!("{} is or holds your home folder", resolved.display()));
    }
    let temp = std::env::temp_dir().canonicalize().ok();
    let in_temp = temp.is_some_and(|temp| resolved.starts_with(temp));
    let denied = SYSTEM
        .iter()
        .filter(|_| !in_temp)
        .map(PathBuf::from)
        .chain(home.iter().flat_map(|home| SECRET.iter().map(move |s| home.join(s))))
        .chain([data_dir.to_path_buf()]);
    for location in denied {
        // A location that exists is compared by its real path (`/etc` is `/private/etc` on macOS).
        let location = location.canonicalize().unwrap_or(location);
        if resolved.starts_with(&location) || location.starts_with(&resolved) {
            return Err(format!("{} is or holds a protected location ({})", resolved.display(), location.display()));
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_system_and_secret_locations() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let data = crate_dir.join("src");
        let media = crate_dir.join("tests");
        assert_eq!(check(&media, &data).unwrap(), media.canonicalize().unwrap());
        for refused in ["/", "/etc", "/usr/bin", "/nonexistent-flicks"] {
            assert!(check(Path::new(refused), &data).is_err(), "{refused}");
        }
        assert!(check(&data, &data).is_err());
        assert!(check(&data.join(".."), &data).is_err());
        let temp = std::env::temp_dir();
        assert_eq!(check(&temp, &data).unwrap(), temp.canonicalize().unwrap());
        if let Some(home) = crate::home_dir() {
            assert!(check(&home, &data).is_err());
            if home.join(".ssh").is_dir() {
                assert!(check(&home.join(".ssh"), &data).is_err());
            }
        }
        // A symlink into a refused location is refused.
        #[cfg(unix)]
        {
            let link = std::env::temp_dir().join(format!("flicks-test-link-{}", std::process::id()));
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink("/etc", &link).unwrap();
            assert!(check(&link, &data).is_err());
            std::fs::remove_file(&link).unwrap();
        }
    }
}
