// Portions of this file are adapted from ecc2 (everything-claude-code).
// Copyright (c) 2026 Affaan Mustafa — MIT License
// Source: https://github.com/affaan-m/everything-claude-code/blob/9a5ed3223aac8b927e5d4a17b6c7c0690eac0b44/ecc2/src/session/daemon.rs
// SPDX-License-Identifier: MIT
//
// Lifted: the `pid_is_alive` helper (lines 476-496 of the upstream file).
// Probes process existence without delivering a signal via `kill(pid, 0)`,
// handling the EPERM case that means "process exists but we can't signal it."

/// Returns true if a process with `pid` exists on this system.
///
/// On Unix, uses `kill(pid, 0)` which checks for the process without
/// sending a signal. `EPERM` is treated as "exists" (we don't have
/// permission to signal it, but it's there).
#[cfg(unix)]
pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    // SAFETY: kill(pid, 0) probes process existence without delivering a signal.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }

    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(code) if code == libc::EPERM
    )
}

#[cfg(not(unix))]
pub fn pid_is_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::pid_is_alive;

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id();
        assert!(pid_is_alive(pid));
    }

    #[test]
    fn pid_zero_is_not_alive() {
        assert!(!pid_is_alive(0));
    }

    #[test]
    fn very_high_pid_is_not_alive() {
        // PIDs above this on Linux/macOS are not realistic; should not exist.
        assert!(!pid_is_alive(u32::MAX - 1));
    }
}
