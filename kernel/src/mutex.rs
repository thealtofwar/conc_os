use core::mem::ManuallyDrop;
use spin::{Mutex, MutexGuard};
use x86_64::instructions::interrupts;

pub struct InterruptMutex<T> {
    inner: Mutex<T>,
}

impl<T> InterruptMutex<T> {
    pub const fn new(data: T) -> Self {
        Self {
            inner: Mutex::new(data),
        }
    }

    pub fn lock(&self) -> InterruptMutexGuard<'_, T> {
        // 1. Save current state and disable interrupts
        let saved_int = interrupts::are_enabled();
        if saved_int {
            interrupts::disable();
        }

        // 2. Safely acquire the inner spinlock now that we won't be interrupted
        InterruptMutexGuard {
            inner_guard: ManuallyDrop::new(self.inner.lock()),
            saved_int,
        }
    }
}

pub struct InterruptMutexGuard<'a, T> {
    inner_guard: ManuallyDrop<MutexGuard<'a, T>>,
    saved_int: bool,
}

impl<'a, T> core::ops::Deref for InterruptMutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner_guard
    }
}

impl<'a, T> core::ops::DerefMut for InterruptMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner_guard
    }
}

impl<'a, T> Drop for InterruptMutexGuard<'a, T> {
    fn drop(&mut self) {
        // STRICT ORDERING REQUIRED:

        // 1. Drop the inner spinlock guard first to release the lock
        unsafe {
            ManuallyDrop::drop(&mut self.inner_guard);
        }

        // 2. Restore interrupts ONLY AFTER the lock is fully released
        if self.saved_int {
            interrupts::enable();
        }
    }
}
