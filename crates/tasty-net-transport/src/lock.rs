//! Mutex locking that survives a panic of another holder. Every state behind these locks stays
//! valid between statements, so a poisoned lock is recovered instead of spreading the panic.

use std::sync::{Mutex, MutexGuard};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
