//! Locking a `std::sync::Mutex` without reading poisoned data.
//!
//! A poisoned mutex means another thread panicked while it held the lock,
//! so the value inside may be half-updated. The Runtime never reaches into
//! a poisoned mutex to use its contents: a fallible path refuses with an
//! internal error ([`lock`]), an access decision fails closed, and pure
//! bookkeeping treats the poison as the bug it is (`expect`).
use loop_protocol::{BackendError, BackendResult, ErrorCategory};
use std::sync::{Mutex, MutexGuard};

/// Lock `mutex`, or refuse when it is poisoned. `what` names the guarded
/// value in the log line and the error. The poisoned guard is dropped
/// unread: the caller gets [`ErrorCategory::Internal`] instead.
pub(crate) fn lock<'a, T>(
    mutex: &'a Mutex<T>,
    what: &'static str,
) -> BackendResult<MutexGuard<'a, T>> {
    mutex.lock().map_err(|_| {
        eprintln!("Runtime: the {what} lock is poisoned (a thread panicked holding it); refusing");
        BackendError::failed(
            ErrorCategory::Internal,
            format!("the {what} lock is poisoned"),
        )
    })
}

/// Poison `mutex`: a thread takes the lock and panics holding it.
#[cfg(test)]
pub(crate) fn poison<T: Send>(mutex: &Mutex<T>) {
    std::thread::scope(|scope| {
        let _ = scope
            .spawn(|| {
                let _guard = mutex.lock().unwrap();
                panic!("poison the lock for a test");
            })
            .join();
    });
    assert!(mutex.is_poisoned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_an_internal_error() {
        let mutex = Mutex::new(1);
        assert_eq!(*lock(&mutex, "test").unwrap(), 1);
        poison(&mutex);
        let refused = lock(&mutex, "test").err();
        match refused {
            Some(BackendError::Failed(info)) => {
                assert_eq!(info.category, ErrorCategory::Internal);
            }
            other => panic!("a poisoned lock was not refused: {other:?}"),
        }
    }
}
