//! Environment workarounds WebKitGTK needs on the proprietary NVIDIA driver.
//!
//! Up to 1.6.2 these were applied by the `webkit2gtk-nvidia-quirk` crate. Its
//! replacement only arms a GL paint context on Wayland, so X11 sessions lost
//! `WEBKIT_DISABLE_DMABUF_RENDERER` and Wayland sessions lost
//! `__NV_DISABLE_EXPLICIT_SYNC`, which left the window blank or only painting
//! the widgets under the cursor. The variables must be set before WebKit or
//! any other thread starts, so `apply` runs first thing in `run`.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

const NVIDIA_VENDOR: u32 = 0x10de;
const DISABLE_DMABUF: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
const DISABLE_EXPLICIT_SYNC: &str = "__NV_DISABLE_EXPLICIT_SYNC";
const OPT_OUT: &str = "RAPIDRAW_NVIDIA_QUIRK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    X11,
    Wayland,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workaround {
    None,
    DisableDmabufRenderer,
    DisableExplicitSync,
}

impl Workaround {
    fn variable(self) -> Option<&'static str> {
        match self {
            Workaround::None => None,
            Workaround::DisableDmabufRenderer => Some(DISABLE_DMABUF),
            Workaround::DisableExplicitSync => Some(DISABLE_EXPLICIT_SYNC),
        }
    }
}

/// The backend GDK will open, which is what WebKit renders through.
pub fn session_type(
    gdk_backend: Option<&str>,
    xdg_session_type: Option<&str>,
    wayland_display: bool,
    x11_display: bool,
) -> Session {
    let from_gdk = gdk_backend.and_then(|list| {
        list.split(',').find_map(|b| match b.trim() {
            "wayland" => Some(Session::Wayland),
            "x11" => Some(Session::X11),
            _ => None,
        })
    });
    if let Some(session) = from_gdk {
        return session;
    }
    match xdg_session_type {
        Some("wayland") if wayland_display => return Session::Wayland,
        Some("x11") if x11_display => return Session::X11,
        _ => {}
    }
    if wayland_display {
        Session::Wayland
    } else if x11_display {
        Session::X11
    } else {
        Session::Unknown
    }
}

pub fn workaround_for(
    nvidia: bool,
    session: Session,
    strict_explicit_sync: bool,
    egl_wayland2: bool,
) -> Workaround {
    if !nvidia {
        return Workaround::None;
    }
    match session {
        Session::X11 => Workaround::DisableDmabufRenderer,
        Session::Wayland if strict_explicit_sync || !egl_wayland2 => {
            Workaround::DisableExplicitSync
        }
        Session::Wayland | Session::Unknown => Workaround::None,
    }
}

/// The variable to set, unless the user opted out or already chose a value.
pub fn variable_to_set(
    workaround: Workaround,
    is_set: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    if is_set(OPT_OUT) {
        return None;
    }
    workaround.variable().filter(|name| !is_set(name))
}

#[cfg(target_os = "linux")]
pub fn apply() {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let session = session_type(
        env("GDK_BACKEND").as_deref(),
        env("XDG_SESSION_TYPE").as_deref(),
        env("WAYLAND_DISPLAY").is_some() || env("WAYLAND_SOCKET").is_some(),
        env("DISPLAY").is_some(),
    );
    let strict = env("XDG_CURRENT_DESKTOP")
        .or_else(|| env("XDG_SESSION_DESKTOP"))
        .is_some_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("hyprland")));
    let nvidia = sysfs::nvidia_drives_display();
    let egl_wayland2 = nvidia && session == Session::Wayland && sysfs::egl_wayland2_active();
    let workaround = workaround_for(nvidia, session, strict, egl_wayland2);
    if let Some(name) = variable_to_set(workaround, |name| std::env::var_os(name).is_some()) {
        eprintln!("NVIDIA driver on {session:?}: setting {name}=1");
        // SAFETY: called at the top of `run`, before any other thread exists.
        unsafe { std::env::set_var(name, "1") };
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply() {}

#[cfg(target_os = "linux")]
mod sysfs {
    use std::path::Path;

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// The boot GPU is an NVIDIA card and the proprietary driver is loaded for
    /// some card (nouveau does not have these problems).
    pub fn nvidia_drives_display() -> bool {
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return false;
        };
        let mut primary_nvidia = false;
        let mut nvidia_driver = false;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let device = entry.path().join("device");
            let vendor = read(&device.join("vendor"))
                .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
            let primary = read(&device.join("boot_vga")).as_deref() == Some("1")
                || read(&device.join("boot_display")).as_deref() == Some("1");
            let driver = std::fs::read_link(device.join("driver"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
            primary_nvidia |= primary && vendor == Some(super::NVIDIA_VENDOR);
            nvidia_driver |= driver.as_deref() == Some("nvidia");
        }
        primary_nvidia && nvidia_driver
    }

    /// egl-wayland2 (driver 560+) handles explicit sync itself.
    pub fn egl_wayland2_active() -> bool {
        let major = read(Path::new("/sys/module/nvidia/version"))
            .and_then(|v| v.split('.').next().and_then(|m| m.parse::<u32>().ok()));
        if major.is_none_or(|m| m < 560) {
            return false;
        }
        let mut manifests: Vec<_> = [
            "/etc/egl/egl_external_platform.d",
            "/usr/share/egl/egl_external_platform.d",
        ]
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.flatten().map(|e| e.path()))
        .collect();
        manifests.sort();
        manifests
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .find(|text| text.contains("nvidia-egl-wayland"))
            .is_some_and(|text| text.contains("nvidia-egl-wayland2"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdk_backend_wins_over_session_type() {
        assert_eq!(
            session_type(Some("x11"), Some("wayland"), true, true),
            Session::X11
        );
        assert_eq!(
            session_type(Some("*,wayland"), Some("x11"), true, true),
            Session::Wayland
        );
        assert_eq!(
            session_type(Some("*"), Some("wayland"), true, true),
            Session::Wayland
        );
    }

    #[test]
    fn session_type_needs_a_display_to_connect_to() {
        assert_eq!(session_type(None, Some("x11"), false, true), Session::X11);
        assert_eq!(
            session_type(None, Some("wayland"), false, true),
            Session::X11
        );
        assert_eq!(session_type(None, None, true, true), Session::Wayland);
        assert_eq!(session_type(None, None, false, false), Session::Unknown);
    }

    #[test]
    fn workarounds_match_the_previous_quirk_crate() {
        use Workaround::*;
        assert_eq!(workaround_for(false, Session::X11, false, false), None);
        assert_eq!(
            workaround_for(true, Session::X11, false, false),
            DisableDmabufRenderer
        );
        assert_eq!(
            workaround_for(true, Session::Wayland, false, false),
            DisableExplicitSync
        );
        assert_eq!(workaround_for(true, Session::Wayland, false, true), None);
        assert_eq!(
            workaround_for(true, Session::Wayland, true, true),
            DisableExplicitSync
        );
        assert_eq!(workaround_for(true, Session::Unknown, true, false), None);
    }

    #[test]
    fn user_choices_are_respected() {
        let none_set = |_: &str| false;
        assert_eq!(
            variable_to_set(Workaround::DisableDmabufRenderer, none_set),
            Some(DISABLE_DMABUF)
        );
        assert_eq!(variable_to_set(Workaround::None, none_set), None);
        assert_eq!(
            variable_to_set(Workaround::DisableDmabufRenderer, |n| n == DISABLE_DMABUF),
            None
        );
        assert_eq!(
            variable_to_set(Workaround::DisableExplicitSync, |n| n == OPT_OUT),
            None
        );
    }
}
