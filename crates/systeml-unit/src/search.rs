//! Unit search paths for user-mode SystemL.
//!
//! Order mirrors `systemd --user`: per-user runtime/state, then user config,
//! then admin user config (`/etc/systemd/user`), then vendor defaults
//! (`/usr/local/lib/systemd/user`, `/usr/lib/systemd/user`).
//!
//! Unit lookup walks paths in order; the first hit wins for the *main*
//! fragment, with all `<name>.<kind>.d/*.conf` drop-ins stacked on top
//! across the entire search path.

use std::path::PathBuf;

/// Default user-mode search paths in priority order.
///
/// `XDG_RUNTIME_DIR` is read from the environment; the SystemL daemon sets
/// this on macOS where the platform doesn't provide one by default.
///
/// We deliberately use **XDG semantics** even on macOS — i.e. `~/.config`
/// rather than `~/Library/Application Support`. home-manager writes its
/// generated systemd unit files to `~/.config/systemd/user/` regardless of
/// platform, and that's the path systemd users on Darwin will keep using.
#[must_use]
pub fn user_search_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(rt) = runtime_dir() {
        out.push(rt.join("systeml/user.control"));
        out.push(rt.join("systemd/user.control"));
        out.push(rt.join("systemd/transient"));
        out.push(rt.join("systemd/generator.early"));
    }
    if let Some(cfg) = xdg_config_home() {
        out.push(cfg.join("systemd/user"));
    }
    if let Some(rt) = runtime_dir() {
        out.push(rt.join("systemd/generator"));
    }
    out.push(PathBuf::from("/etc/systemd/user"));
    if let Some(rt) = runtime_dir() {
        out.push(rt.join("systemd/user"));
        out.push(rt.join("systemd/generator.late"));
    }
    out.push(PathBuf::from("/usr/local/lib/systemd/user"));
    out.push(PathBuf::from("/usr/local/share/systemd/user"));
    out.push(PathBuf::from("/usr/lib/systemd/user"));
    out.push(PathBuf::from("/usr/share/systemd/user"));
    out
}

/// `$XDG_CONFIG_HOME` or `~/.config`, in that order. Bypasses
/// `dirs::config_dir()` which returns `~/Library/Application Support` on
/// macOS — we want XDG semantics everywhere.
#[must_use]
pub fn xdg_config_home() -> Option<PathBuf> {
    if let Some(c) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(c));
    }
    dirs::home_dir().map(|h| h.join(".config"))
}

/// `$XDG_RUNTIME_DIR` if set. On macOS, the SystemL daemon ensures this is
/// populated before any consumer runs.
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
}

/// `$XDG_STATE_HOME` or `~/.local/state`.
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    if let Some(s) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(s));
    }
    dirs::home_dir().map(|h| h.join(".local/state"))
}

/// `<state_dir>/systeml`.
#[must_use]
pub fn systeml_state_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join("systeml"))
}

/// The directory where `systemlctl enable` writes WantedBy/RequiredBy
/// symlinks. Matches systemd: `$XDG_CONFIG_HOME/systemd/user`.
#[must_use]
pub fn user_config_dir() -> Option<PathBuf> {
    xdg_config_home().map(|c| c.join("systemd/user"))
}
