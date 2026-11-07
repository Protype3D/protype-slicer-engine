//! What tells one computer from another, for the project lock (spec §6.7).
//!
//! The per-install `machine_id` next to the app data is minted per HOME: a
//! dev build, an agent sandbox and a CI runner on the same Mac each carry
//! their own, and a lock left behind by a killed daemon of one of them
//! looked like another machine's to the rest — although its pid could have
//! been probed right here. The OS keeps one identifier per host; this module
//! reads it, and the host's name for the message shown to a person on the
//! other machine.

use std::sync::OnceLock;

/// The OS's stable identifier of this host: `IOPlatformUUID` on macOS,
/// `/etc/machine-id` on Linux, `MachineGuid` on Windows. `None` where none
/// can be read — the lock then falls back to comparing install ids, as it
/// did before the host id existed.
///
/// Read once per process: macOS and Windows answer through a child process.
pub fn host_id() -> Option<&'static str> {
    static HOST_ID: OnceLock<Option<String>> = OnceLock::new();
    HOST_ID.get_or_init(read_host_id).as_deref()
}

/// The host's name, as a person would recognise it in "open on …".
#[cfg(target_family = "unix")]
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` outlives the call and its length is passed alongside;
    // gethostname writes at most that many bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    non_empty(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// The host's name, as a person would recognise it in "open on …".
#[cfg(not(target_family = "unix"))]
pub fn hostname() -> Option<String> {
    non_empty(std::env::var("COMPUTERNAME").ok()?)
}

#[cfg(target_os = "macos")]
fn read_host_id() -> Option<String> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // One line of the listing reads `"IOPlatformUUID" = "…"`.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            if key.trim() != "\"IOPlatformUUID\"" {
                return None;
            }
            non_empty(value.trim().trim_matches('"').to_string())
        })
}

#[cfg(target_os = "linux")]
fn read_host_id() -> Option<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|path| non_empty(std::fs::read_to_string(path).ok()?))
}

#[cfg(target_os = "windows")]
fn read_host_id() -> Option<String> {
    use std::os::windows::process::CommandExt;
    // The daemon runs without a console; a console child would otherwise
    // open one (see the host's `spawn_and_wait_ready` for the same flag).
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // The value line reads `    MachineGuid    REG_SZ    <guid>`.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? != "MachineGuid" {
                return None;
            }
            let _kind = parts.next()?;
            non_empty(parts.next()?.to_string())
        })
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn read_host_id() -> Option<String> {
    None
}

fn non_empty(s: String) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_is_stable_within_the_process() {
        assert_eq!(host_id(), host_id());
    }

    // The two platforms the lock is relied on: without a host id there the
    // stale-lock check would silently fall back to the per-install id.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn host_id_is_known_on_mac_and_linux() {
        assert!(
            host_id().is_some_and(|id| !id.is_empty()),
            "{:?}",
            host_id()
        );
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn hostname_is_known_on_unix() {
        assert!(hostname().is_some_and(|name| !name.is_empty()));
    }
}
