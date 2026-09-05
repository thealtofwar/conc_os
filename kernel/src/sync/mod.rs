//! Synchronisation primitives.
//!
//! The kernel runs on a single CPU today, but interrupt handlers can run
//! concurrently with everything else, so every lock disables interrupts for
//! the duration of the critical section.

#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::cpu;

/// An interrupt-safe spinlock.
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    restore_if: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(v: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }

    #[inline]
    pub fn lock(&self) -> SpinGuard<'_, T> {
        let restore_if = cpu::save_and_disable_interrupts();
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.locked.load(Ordering::Relaxed) {
                cpu::pause();
            }
        }
        SpinGuard { lock: self, restore_if }
    }

    #[inline]
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let restore_if = cpu::save_and_disable_interrupts();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Some(SpinGuard { lock: self, restore_if })
        } else {
            cpu::restore_interrupts(restore_if);
            None
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Break a lock, e.g. from the panic handler.  Only sound when the holder
    /// is known to be dead.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}
impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        cpu::restore_interrupts(self.restore_if);
    }
}

/// A cell that is written exactly once during boot and read afterwards.
pub struct OnceCell<T> {
    set: AtomicBool,
    data: UnsafeCell<MaybeUninit<T>>,
}

unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}
unsafe impl<T: Send> Send for OnceCell<T> {}

impl<T> OnceCell<T> {
    pub const fn new() -> Self {
        OnceCell { set: AtomicBool::new(false), data: UnsafeCell::new(MaybeUninit::uninit()) }
    }
    /// Initialise the cell.  Panics if already initialised.
    pub fn init(&self, v: T) {
        if self.set.load(Ordering::Acquire) {
            panic!("OnceCell initialised twice");
        }
        unsafe { (*self.data.get()).write(v) };
        self.set.store(true, Ordering::Release);
    }
    pub fn get(&self) -> Option<&T> {
        if self.set.load(Ordering::Acquire) {
            Some(unsafe { (*self.data.get()).assume_init_ref() })
        } else {
            None
        }
    }
    pub fn is_set(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }
    #[track_caller]
    pub fn expect(&self, what: &str) -> &T {
        match self.get() {
            Some(v) => v,
            None => panic!("{} used before initialisation", what),
        }
    }
}

impl<T> Deref for OnceCell<T> {
    type Target = T;
    #[track_caller]
    fn deref(&self) -> &T {
        self.expect("OnceCell")
    }
}

/// Run a closure with interrupts disabled.
#[inline]
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let restore = cpu::save_and_disable_interrupts();
    let r = f();
    cpu::restore_interrupts(restore);
    r
}
