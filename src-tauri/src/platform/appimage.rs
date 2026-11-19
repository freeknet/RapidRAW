//! The AppImages bundle `libwayland-client.so.0` from the Ubuntu build host.
//! Newer Mesa (26.x on Fedora 44 and Arch) needs symbols that old copy lacks,
//! so Mesa's EGL fails to load: wgpu finds no adapter and WebKitWebProcess
//! aborts with `EGL_BAD_PARAMETER`. The library is already loaded by the time
//! `run` starts, so when the host has its own copy the process re-executes
//! itself once with that copy preloaded. WebKit's child processes inherit it.
//! `RAPIDRAW_APPIMAGE_HOST_WAYLAND=0` turns it off.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const LIBRARY: &str = "libwayland-client.so.0";
const MARKER: &str = "RAPIDRAW_APPIMAGE_HOST_WAYLAND";

#[cfg(target_arch = "aarch64")]
const HOST_DIRS: [&str; 5] = [
    "/usr/lib64",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib",
    "/lib64",
    "/lib/aarch64-linux-gnu",
];
#[cfg(not(target_arch = "aarch64"))]
const HOST_DIRS: [&str; 5] = [
    "/usr/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib",
    "/lib64",
    "/lib/x86_64-linux-gnu",
];

/// The host library to preload, if this is an AppImage that bundles its own
/// copy and nothing has been decided yet.
pub fn host_library(
    appdir: Option<&Path>,
    marker: Option<&OsStr>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if marker.is_some() {
        return None;
    }
    let appdir = appdir?;
    if !exists(&appdir.join("usr/lib").join(LIBRARY)) {
        return None;
    }
    HOST_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(LIBRARY))
        .find(|path| !path.starts_with(appdir) && exists(path))
}

/// `LD_PRELOAD` with the host library in front of whatever was already there.
pub fn preload_value(library: &Path, existing: Option<&OsStr>) -> OsString {
    let mut value = OsString::from(library);
    if let Some(existing) = existing.filter(|v| !v.is_empty()) {
        value.push(":");
        value.push(existing);
    }
    value
}

#[cfg(target_os = "linux")]
pub fn prefer_host_wayland() {
    use std::os::unix::process::CommandExt;

    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    let marker = std::env::var_os(MARKER);
    let Some(library) = host_library(appdir.as_deref(), marker.as_deref(), Path::exists) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut args = std::env::args_os();
    let arg0 = args.next().unwrap_or_else(|| exe.clone().into_os_string());
    let preload = preload_value(&library, std::env::var_os("LD_PRELOAD").as_deref());
    eprintln!(
        "AppImage: restarting with the system {} ({})",
        LIBRARY,
        library.display()
    );
    let error = std::process::Command::new(&exe)
        .arg0(arg0)
        .args(args)
        .env("LD_PRELOAD", preload)
        .env(MARKER, "1")
        .exec();
    // Only reached when exec failed; carry on with the bundled library.
    eprintln!("AppImage: restart failed, using the bundled library: {error}");
}

#[cfg(not(target_os = "linux"))]
pub fn prefer_host_wayland() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled(path: &Path) -> bool {
        path == Path::new("/tmp/.mount_x/usr/lib/libwayland-client.so.0")
    }

    #[test]
    fn prefers_the_host_copy_inside_an_appimage() {
        let appdir = Path::new("/tmp/.mount_x");
        let found = host_library(Some(appdir), None, |p| {
            bundled(p) || p == Path::new("/usr/lib64/libwayland-client.so.0")
        });
        assert_eq!(
            found.as_deref(),
            Some(Path::new("/usr/lib64/libwayland-client.so.0"))
        );
    }

    #[test]
    fn leaves_other_installs_alone() {
        let host = |p: &Path| p == Path::new("/usr/lib/libwayland-client.so.0");
        assert_eq!(host_library(None, None, host), None);
        let appdir = Path::new("/tmp/.mount_x");
        assert_eq!(host_library(Some(appdir), None, host), None);
        assert_eq!(
            host_library(Some(appdir), Some(OsStr::new("0")), |p| bundled(p)
                || host(p)),
            None
        );
        assert_eq!(host_library(Some(appdir), None, bundled), None);
    }

    #[test]
    fn keeps_an_existing_preload() {
        let lib = Path::new("/usr/lib/libwayland-client.so.0");
        assert_eq!(preload_value(lib, None), "/usr/lib/libwayland-client.so.0");
        assert_eq!(
            preload_value(lib, Some(OsStr::new("libfoo.so"))),
            "/usr/lib/libwayland-client.so.0:libfoo.so"
        );
    }
}
