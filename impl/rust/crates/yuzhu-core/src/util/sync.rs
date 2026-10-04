//! `Mutex` / `RwLock` helpers that turn lock poisoning into a
//! `Severity::Panic` error (`m2.md` §2, convention 7).
//!
//! A poisoned lock means another thread panicked while holding it; the
//! protected state may be half-updated, so the cluster must stop.

use std::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use crate::error::{Error, Result, Severity, sqlstate};

fn poisoned() -> Error {
    Error::new(sqlstate::INTERNAL_ERROR, "lock poisoned").with_severity(Severity::Panic)
}

pub fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock().map_err(|_| poisoned())
}

pub fn read<T>(l: &RwLock<T>) -> Result<RwLockReadGuard<'_, T>> {
    l.read().map_err(|_| poisoned())
}

pub fn write<T>(l: &RwLock<T>) -> Result<RwLockWriteGuard<'_, T>> {
    l.write().map_err(|_| poisoned())
}

/// `Condvar::wait` with poison mapped to a Panic error.
pub fn wait<'a, T>(cv: &Condvar, g: MutexGuard<'a, T>) -> Result<MutexGuard<'a, T>> {
    cv.wait(g).map_err(|_| poisoned())
}

/// `Condvar::wait_timeout`; the flag is true when the wait timed out.
pub fn wait_timeout<'a, T>(
    cv: &Condvar,
    g: MutexGuard<'a, T>,
    dur: Duration,
) -> Result<(MutexGuard<'a, T>, bool)> {
    cv.wait_timeout(g, dur)
        .map(|(g, r)| (g, r.timed_out()))
        .map_err(|_| poisoned())
}

/// For use inside `Drop` only: continues with the inner guard even if the
/// lock is poisoned (the panic was already handled elsewhere).
pub fn lock_ignore_poison<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn healthy_locks_work() {
        let m = Mutex::new(1);
        *lock(&m).unwrap() += 1;
        assert_eq!(*lock(&m).unwrap(), 2);
        let l = RwLock::new(5);
        *write(&l).unwrap() = 6;
        assert_eq!(*read(&l).unwrap(), 6);
    }

    #[test]
    fn poisoned_locks_become_panic_errors() {
        let m = Arc::new(Mutex::new(0));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison");
        })
        .join();
        let e = lock(&m).unwrap_err();
        assert_eq!(e.severity, Severity::Panic);
        assert_eq!(e.sqlstate, sqlstate::INTERNAL_ERROR);
        // Drop-time access still works.
        assert_eq!(*lock_ignore_poison(&m), 0);

        let l = Arc::new(RwLock::new(0));
        let l2 = Arc::clone(&l);
        let _ = std::thread::spawn(move || {
            let _g = l2.write().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(read(&l).unwrap_err().severity, Severity::Panic);
        assert_eq!(write(&l).unwrap_err().severity, Severity::Panic);
    }

    #[test]
    fn wait_timeout_reports_timeout() {
        let m = Mutex::new(());
        let cv = Condvar::new();
        let g = lock(&m).unwrap();
        let (_g, timed_out) = wait_timeout(&cv, g, Duration::from_millis(5)).unwrap();
        assert!(timed_out);
    }
}
