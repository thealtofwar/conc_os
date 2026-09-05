//! Shell commands and self-test suites contributed by subsystems.
//!
//! The core shell (`shell.rs`) handles kernel-level commands; everything
//! device- or hypervisor-related is routed through here so each subsystem
//! stays self-contained.

use alloc::vec::Vec;

use crate::selftest::TestFn;

pub fn help() {
    crate::net::help();
    crate::disk::help();
    crate::hv::help();
}

/// Try to run `cmd`.  Returns false if no subsystem recognises it.
pub async fn dispatch(cmd: &str, args: &[&str]) -> bool {
    if crate::net::dispatch(cmd, args).await {
        return true;
    }
    if crate::disk::dispatch(cmd, args).await {
        return true;
    }
    if crate::hv::dispatch(cmd, args).await {
        return true;
    }
    false
}

/// Additional self-test suites: (suite name, tests).
pub fn test_suites() -> Vec<(&'static str, &'static [(&'static str, TestFn)])> {
    alloc::vec![("net", crate::net::tests()), ("disk", crate::disk::tests()), ("hv", crate::hv::test_suite())]
}
