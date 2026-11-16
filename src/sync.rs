use std::sync::{Mutex, MutexGuard};

/// Extension trait for locking a mutex even after another thread panicked
/// while holding it.
///
/// The caches and progress state in this crate only contain recoverable data,
/// so a poisoned lock should not turn every later operation into a panic. The
/// lock is marked healthy again after recovery.
pub trait MutexExt<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|error| {
            self.clear_poison();
            error.into_inner()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn recovers_after_a_poisoned_lock() {
        let mutex = Arc::new(Mutex::new(7));
        let poisoned = Arc::clone(&mutex);
        let _ = thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();

        assert!(mutex.is_poisoned());
        assert_eq!(*mutex.lock_unpoisoned(), 7);
        assert!(!mutex.is_poisoned());
    }
}
