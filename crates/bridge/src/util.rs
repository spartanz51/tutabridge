use std::sync::{Mutex, MutexGuard};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
