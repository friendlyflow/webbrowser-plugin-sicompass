//! Finding Chrome (and Xvfb) by name, the way a desktop user expects.
//!
//! In order: the folders on `PATH`; `~/.local/bin`, where per-user installers
//! put programs and which a desktop session's `PATH` often lacks; on macOS the
//! application bundles (`/Applications/<name>.app/Contents/MacOS/<name>`, then
//! the same under `~/Applications`), since a GUI browser is not on `PATH`
//! there; and on Windows the folders the browsers' own installers use, under
//! Program Files and the user's LocalAppData.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where `program` (a name, never a path) would start from, or `None` when
/// it is nowhere.
pub fn resolve(program: &str) -> Option<PathBuf> {
    if program.is_empty() || program.contains(['/', '\\']) {
        return None;
    }
    let home = home_dir();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let installed = if cfg!(windows) {
        windows_install_paths(program, |k| std::env::var_os(k))
    } else {
        Vec::new()
    };
    find(program, &path, home.as_deref())
        .or_else(|| installed.into_iter().find(|p| is_executable(p)))
}

/// The user's home folder.
fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// [`resolve`] on `PATH`, `~/.local/bin` and the macOS bundles, with `PATH`
/// and the home folder handed in.
fn find(program: &str, path: &std::ffi::OsStr, home: Option<&Path>) -> Option<PathBuf> {
    let own_bin = home.map(|h| h.join(".local").join("bin"));
    let bundles: Vec<PathBuf> = if cfg!(target_os = "macos") {
        std::iter::once(PathBuf::from("/Applications"))
            .chain(home.map(|h| h.join("Applications")))
            .map(|root| app_bundle_dir(&root, program))
            .collect()
    } else {
        Vec::new()
    };
    std::env::split_paths(path)
        .chain(own_bin)
        .chain(bundles)
        .flat_map(|dir| candidates(&dir, program))
        .find(|c| is_executable(c))
}

/// Where a macOS application named `program` keeps its executable under
/// `root`: `<root>/<program>.app/Contents/MacOS`. So `Google Chrome` is found
/// as a name, like any other program.
fn app_bundle_dir(root: &Path, program: &str) -> PathBuf {
    root.join(format!("{program}.app"))
        .join("Contents")
        .join("MacOS")
}

/// Where the Windows installers put the browsers named `program`, under the
/// install roots `env` names (Program Files in both spellings, and the user's
/// LocalAppData for a per-user install).
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_install_paths(program: &str, env: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    const MACHINE: &[&str] = &["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"];
    const USER: &[&str] = &["LOCALAPPDATA"];
    let (rel, roots): (&[&str], Vec<&str>) = match program {
        "Google Chrome" => (
            &["Google", "Chrome", "Application", "chrome.exe"],
            [MACHINE, USER].concat(),
        ),
        "Google Chrome Canary" => (
            &["Google", "Chrome SxS", "Application", "chrome.exe"],
            USER.to_vec(),
        ),
        "Chromium" => (
            &["Chromium", "Application", "chrome.exe"],
            [USER, MACHINE].concat(),
        ),
        "Microsoft Edge" => (
            &["Microsoft", "Edge", "Application", "msedge.exe"],
            MACHINE.to_vec(),
        ),
        _ => return Vec::new(),
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots
        .iter()
        .filter_map(|k| env(k))
        .filter(|r| !r.is_empty())
    {
        let p = rel.iter().fold(PathBuf::from(root), |p, c| p.join(c));
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// The files `program` can be in `dir`.
#[cfg(not(windows))]
fn candidates(dir: &Path, program: &str) -> Vec<PathBuf> {
    vec![dir.join(program)]
}

/// On Windows, with each `PATHEXT` extension too (`chrome` is `chrome.exe`).
#[cfg(windows)]
fn candidates(dir: &Path, program: &str) -> Vec<PathBuf> {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    std::iter::once(dir.join(program))
        .chain(
            exts.split(';')
                .filter(|e| !e.is_empty())
                .map(|e| dir.join(format!("{program}{}", e.to_ascii_lowercase()))),
        )
        .collect()
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn executable(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn path_comes_first_then_local_bin() {
        let home = tempfile::tempdir().unwrap();
        let on_path = tempfile::tempdir().unwrap();
        let local = executable(&home.path().join(".local").join("bin"), "chromium");
        let path = std::env::join_paths([on_path.path()]).unwrap();
        assert_eq!(find("chromium", &path, Some(home.path())), Some(local));

        let first = executable(on_path.path(), "chromium");
        assert_eq!(find("chromium", &path, Some(home.path())), Some(first));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_executable_is_not_the_program() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("chromium"), "not a program").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(find("chromium", &path, None), None);
    }

    #[test]
    fn a_path_or_nothing_is_never_looked_up() {
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("/usr/bin/chromium"), None);
        assert_eq!(resolve(r"C:\chrome.exe"), None);
    }

    #[test]
    fn a_mac_application_is_found_in_its_bundle() {
        assert_eq!(
            app_bundle_dir(Path::new("/Applications"), "Google Chrome"),
            Path::new("/Applications/Google Chrome.app/Contents/MacOS")
        );
    }

    #[test]
    fn the_windows_browsers_are_looked_for_where_their_installers_put_them() {
        let env = |k: &str| match k {
            "ProgramFiles" => Some(OsString::from("PF")),
            "ProgramFiles(x86)" => Some(OsString::from("PF86")),
            "ProgramW6432" => Some(OsString::from("PF")),
            "LOCALAPPDATA" => Some(OsString::from("LAD")),
            _ => None,
        };
        let chrome = windows_install_paths("Google Chrome", env);
        let p = |parts: &[&str]| parts.iter().collect::<PathBuf>();
        assert_eq!(
            chrome,
            [
                p(&["PF", "Google", "Chrome", "Application", "chrome.exe"]),
                p(&["PF86", "Google", "Chrome", "Application", "chrome.exe"]),
                p(&["LAD", "Google", "Chrome", "Application", "chrome.exe"]),
            ]
        );
        assert_eq!(
            windows_install_paths("Microsoft Edge", env)[0],
            p(&["PF", "Microsoft", "Edge", "Application", "msedge.exe"])
        );
        assert_eq!(
            windows_install_paths("Google Chrome Canary", env),
            [p(&[
                "LAD",
                "Google",
                "Chrome SxS",
                "Application",
                "chrome.exe"
            ])]
        );
        assert_eq!(windows_install_paths("Chromium", env).len(), 3);
        // A Linux command name has no Windows install folder.
        assert!(windows_install_paths("google-chrome", env).is_empty());
        // Unset roots are skipped.
        assert!(windows_install_paths("Google Chrome", |_| None).is_empty());
    }
}
