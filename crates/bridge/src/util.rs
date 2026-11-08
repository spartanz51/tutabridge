use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock a std `Mutex`, recovering the guard if a previous holder poisoned it by
/// panicking. Our locked sections are short and panic-free, so this should
/// never trigger; recovering instead of `unwrap()` keeps one stray panic from
/// cascading into every later lock. It matters most for the event-bus
/// `last_batch_ids` map, which is shared with the SDK's reconnect path: a
/// poison there would otherwise make every reconnect panic and kill realtime
/// sync for the rest of the process's life.
pub fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// [`lock_recover`] for a std `RwLock` read guard.
pub fn rwlock_read_recover<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// [`lock_recover`] for a std `RwLock` write guard.
pub fn rwlock_write_recover<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Compare a secret a client presented against the expected one without
/// leaking, through timing, how many leading bytes matched. `==` on strings
/// stops at the first differing byte; `subtle` always looks at every byte
/// (a length mismatch is a plain `false`, since the length of the bridge
/// password is not secret).
pub fn secret_eq(presented: &str, expected: &str) -> bool {
    use subtle::ConstantTimeEq;
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Keep the process's memory out of reach of other processes running as the
/// same user. The bridge holds the Tuta session, the cache key and decrypted
/// mail in memory, so on Linux it clears the dumpable flag: the kernel then
/// refuses `ptrace` attach (and so `/proc/<pid>/mem`) from unprivileged
/// processes, and writes no core dump if the bridge crashes. Root is not
/// affected. A no-op on other platforms. Call it once, early in `main`.
pub fn harden_process() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: PR_SET_DUMPABLE takes one integer argument and touches no
        // memory of ours.
        let rc = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong, 0, 0, 0) };
        if rc != 0 {
            log::warn!(
                "Could not make the process non-dumpable: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs in a child process, so the test binary itself stays dumpable.
    #[cfg(target_os = "linux")]
    #[test]
    fn harden_process_makes_the_process_non_dumpable() {
        const CHILD: &str = "TUTABRIDGE_HARDEN_PROCESS_CHILD";
        const NAME: &str = "util::tests::harden_process_makes_the_process_non_dumpable";
        if std::env::var_os(CHILD).is_some() {
            harden_process();
            // SAFETY: PR_GET_DUMPABLE takes no argument and returns the flag.
            let dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
            assert_eq!(dumpable, 0, "the process is still dumpable");
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "child failed: {stdout}");
        assert!(stdout.contains("1 passed"), "child ran no test: {stdout}");
    }
    use std::sync::{Arc, Mutex};

    #[test]
    fn secret_eq_matches_only_identical_secrets() {
        assert!(secret_eq("abcde-fghjk", "abcde-fghjk"));
        assert!(!secret_eq("abcde-fghjk", "abcde-fghjX"));
        assert!(
            !secret_eq("abcde", "abcde-fghjk"),
            "a prefix is not a match"
        );
        assert!(!secret_eq("", "abcde"));
        assert!(secret_eq("", ""));
    }

    #[test]
    fn recovers_from_a_poisoned_mutex() {
        let m = Arc::new(Mutex::new(0u32));
        let m2 = m.clone();
        // Poison it: panic while holding the guard.
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison");
        })
        .join();

        assert!(m.lock().is_err(), "mutex should now be poisoned");
        // unwrap() would panic here; lock_recover hands back a usable guard.
        let mut g = lock_recover(&m);
        *g += 1;
        assert_eq!(*g, 1);
    }
}
