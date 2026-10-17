//! Bringing forward the terminal tab where a session runs. Off unless ccorbit starts with
//! `--allow-jump`: it is the one feature that runs a command (`open`) and reads beyond
//! `~/.claude` (the environment of that session's own process, for the focus link its terminal
//! gave it). Only two variables are kept from that environment, never stored or shown:
//! `WARP_FOCUS_URL` (the exact Warp tab) and `__CFBundleIdentifier` (the terminal app).

use std::process::{Command, Stdio};

/// How a session's terminal can be reached.
pub enum Target {
    /// The exact Warp tab, by its focus link.
    Tab(String),
    /// Only the terminal app, by its bundle id.
    App(String),
}

/// Finds how to reach a session's terminal, without doing anything yet.
pub fn target(pid: i64) -> Result<Target, &'static str> {
    if !cfg!(target_os = "macos") {
        return Err("jumping to a tab works on macOS only");
    }
    let env = environ(pid).ok_or("cannot read that session's process")?;
    let get = |k: &str| env.iter().find_map(|e| e.strip_prefix(k).and_then(|v| v.strip_prefix('='))).map(str::to_string);
    if let Some(url) = get("WARP_FOCUS_URL").filter(|u| focus_url(u)) {
        return Ok(Target::Tab(url));
    }
    if let Some(bundle) = get("__CFBundleIdentifier").filter(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')) {
        return Ok(Target::App(bundle));
    }
    Err("its terminal gave no way to reach the tab")
}

/// Brings the terminal forward. What happened, for a notice.
pub fn jump(pid: i64) -> Result<&'static str, &'static str> {
    match target(pid)? {
        Target::Tab(url) => open(&[&url]).map(|_| "brought its Warp tab forward"),
        Target::App(bundle) => open(&["-b", &bundle]).map(|_| "brought its terminal forward (not the exact tab)"),
    }
}

/// `warp://session/<32 hex>` (or `warppreview://…`): the only links that are followed.
fn focus_url(u: &str) -> bool {
    let Some((scheme, rest)) = u.split_once("://") else { return false };
    let Some(id) = rest.strip_prefix("session/") else { return false };
    scheme.starts_with("warp")
        && scheme.chars().all(|c| c.is_ascii_lowercase())
        && id.len() == 32
        && id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn open(args: &[&str]) -> Result<(), &'static str> {
    // No shell: the arguments go to `open` as they are.
    match Command::new("/usr/bin/open").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status() {
        Ok(s) if s.success() => Ok(()),
        _ => Err("the terminal did not answer"),
    }
}

/// The environment of one of the user's own processes, read with `sysctl(KERN_PROCARGS2)`: the
/// argument count, the executable path, the arguments, then the environment, NUL-separated.
#[cfg(target_os = "macos")]
fn environ(pid: i64) -> Option<Vec<String>> {
    unsafe extern "C" {
        fn sysctl(name: *mut i32, namelen: u32, oldp: *mut u8, oldlenp: *mut usize, newp: *mut u8, newlen: usize) -> i32;
    }
    const CTL_KERN: i32 = 1;
    const KERN_ARGMAX: i32 = 8;
    const KERN_PROCARGS2: i32 = 49;
    let pid = i32::try_from(pid).ok().filter(|&p| p > 0)?;
    let mut argmax: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    let mut mib = [CTL_KERN, KERN_ARGMAX];
    // SAFETY: the buffer is an i32 and `len` its size.
    if unsafe { sysctl(mib.as_mut_ptr(), 2, (&mut argmax as *mut i32).cast(), &mut len, std::ptr::null_mut(), 0) } != 0 || argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    let mut len = buf.len();
    let mut mib = [CTL_KERN, KERN_PROCARGS2, pid];
    // SAFETY: `buf` holds `len` bytes; the kernel writes at most that many and updates `len`.
    if unsafe { sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr(), &mut len, std::ptr::null_mut(), 0) } != 0 {
        return None;
    }
    buf.truncate(len);
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    // The executable path, then NUL padding, then the arguments and the environment.
    let path_end = rest.iter().position(|&b| b == 0)?;
    let rest = &rest[path_end..];
    let start = rest.iter().position(|&b| b != 0)?;
    let strings: Vec<String> = rest[start..].split(|&b| b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    Some(
        strings
            .into_iter()
            .skip(argc)
            .take_while(|s| !s.is_empty())
            .filter(|s| s.starts_with("WARP_FOCUS_URL=") || s.starts_with("__CFBundleIdentifier="))
            .collect(),
    )
}

#[cfg(not(target_os = "macos"))]
fn environ(_pid: i64) -> Option<Vec<String>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_only_warp_session_links() {
        assert!(focus_url("warp://session/efdf0ea6cfc34fcfb20545238a055d49"));
        assert!(focus_url("warppreview://session/efdf0ea6cfc34fcfb20545238a055d49"));
        assert!(!focus_url("warp://session/efdf0ea6"));
        assert!(!focus_url("https://session/efdf0ea6cfc34fcfb20545238a055d49"));
        assert!(!focus_url("warp://action/new_tab?path=/"));
        assert!(!focus_url("warp://session/EFDF0EA6CFC34FCFB20545238A055D49"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_its_own_environment() {
        // The test process reads itself; only the two variables kept are returned.
        let env = environ(std::process::id() as i64).expect("own process");
        assert!(env.iter().all(|e| e.starts_with("WARP_FOCUS_URL=") || e.starts_with("__CFBundleIdentifier=")));
    }
}
