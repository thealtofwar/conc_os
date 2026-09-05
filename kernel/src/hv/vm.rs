//! The shared, thread-safe view of a VM: its state, request queue, control
//! commands, console and statistics.  The heavy machine state (VMCB,
//! registers, guest memory, device model) is owned exclusively by the VM's
//! vCPU task (`vcpu.rs`); everything else in the system talks to a VM through
//! this handle.

#![allow(dead_code)]

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;

use crate::sync::SpinLock;
use crate::task::{timer, Notify};
use crate::time;

/// Service kinds understood by the unikernel guest image (see `guest/src/main.rs`).
pub mod kind {
    pub const ECHO: u64 = 0;
    pub const PRIMES: u64 = 1;
    pub const COUNTER: u64 = 2;
    pub const SPIN: u64 = 3;
    pub const FAULT: u64 = 4;
    pub const SLEEPY: u64 = 5;
    pub const HELLO: u64 = 6;

    pub fn name(k: u64) -> &'static str {
        match k {
            ECHO => "echo",
            PRIMES => "primes",
            COUNTER => "counter",
            SPIN => "spin",
            FAULT => "fault",
            SLEEPY => "sleepy",
            HELLO => "hello",
            _ => "?",
        }
    }

    pub fn parse(s: &str) -> Option<u64> {
        Some(match s {
            "echo" => ECHO,
            "primes" => PRIMES,
            "counter" => COUNTER,
            "spin" => SPIN,
            "fault" => FAULT,
            "sleepy" => SLEEPY,
            "hello" => HELLO,
            _ => return s.parse().ok(),
        })
    }
}

/// What kind of guest a VM runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmKind {
    /// The bundled request/response unikernel running service `kind`.
    Unikernel(u64),
    /// A Linux kernel with the legacy PC device model.
    Linux,
}

impl VmKind {
    pub fn is_linux(&self) -> bool {
        matches!(self, VmKind::Linux)
    }
    pub fn service_kind(&self) -> u64 {
        match self {
            VmKind::Unikernel(k) => *k,
            VmKind::Linux => 0,
        }
    }
}

impl core::fmt::Display for VmKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VmKind::Unikernel(k) => write!(f, "{}", kind::name(*k)),
            VmKind::Linux => write!(f, "linux"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmState {
    /// Created, has not run yet.
    Created,
    /// Runnable or running on the CPU.
    Running,
    /// Blocked waiting for a request (unikernel) or halted waiting for an
    /// interrupt (Linux): zero CPU, memory warm.
    Idle,
    /// Blocked in a timed sleep.
    Sleeping,
    /// Idle with its memory evicted to disk: zero CPU and RAM.
    Frozen,
    /// Guest exited or halted for good.
    Exited(u64),
    /// Guest faulted; details in `crash_reason`.
    Crashed,
    /// Destroyed by the host.
    Killed,
}

impl VmState {
    pub fn is_finished(&self) -> bool {
        matches!(self, VmState::Exited(_) | VmState::Crashed | VmState::Killed)
    }
    pub fn is_blocked(&self) -> bool {
        matches!(self, VmState::Idle | VmState::Frozen)
    }
}

impl core::fmt::Display for VmState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VmState::Created => write!(f, "created"),
            VmState::Running => write!(f, "running"),
            VmState::Idle => write!(f, "idle"),
            VmState::Sleeping => write!(f, "sleeping"),
            VmState::Frozen => write!(f, "frozen"),
            VmState::Exited(c) => write!(f, "exited({})", c),
            VmState::Crashed => write!(f, "crashed"),
            VmState::Killed => write!(f, "killed"),
        }
    }
}

/// Where a request's answer goes.
pub struct Reply {
    slot: SpinLock<Option<Vec<u8>>>,
    notify: Notify,
}

impl Reply {
    pub fn new() -> Arc<Reply> {
        Arc::new(Reply { slot: SpinLock::new(None), notify: Notify::new() })
    }
    pub fn set(&self, v: Vec<u8>) {
        *self.slot.lock() = Some(v);
        self.notify.notify_one();
    }
    pub fn try_take(&self) -> Option<Vec<u8>> {
        self.slot.lock().take()
    }
    pub async fn wait(&self, timeout_ms: u64) -> Option<Vec<u8>> {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        loop {
            if let Some(v) = self.try_take() {
                return Some(v);
            }
            let now = time::now();
            if now >= deadline {
                return None;
            }
            let remaining = time::tsc_to_us(deadline - now) / 1000 + 1;
            let _ = timer::timeout(remaining, self.notify.notified()).await;
        }
    }
}

impl Default for Reply {
    fn default() -> Self {
        Reply { slot: SpinLock::new(None), notify: Notify::new() }
    }
}

pub struct Request {
    pub data: Vec<u8>,
    pub reply: Option<Arc<Reply>>,
    pub enqueued: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Freeze,
    Thaw,
    Kill,
    /// Reboot the guest from its template.
    Reset,
    /// Print device-model state to the console.
    Dump,
    /// Capture memory and CPU/device state into a named snapshot template
    /// (Linux VMs, taken while halted); the result is reported through
    /// `VmHandle::wait_snapshot`.
    Snapshot(String),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VmStats {
    pub runs: u64,
    pub exits: u64,
    pub npf: u64,
    pub cow: u64,
    pub hcalls: u64,
    pub intr: u64,
    pub cpuid: u64,
    pub io: u64,
    pub msr: u64,
    pub mmio: u64,
    pub injected: u64,
    pub halts: u64,
    pub resets: u64,
    pub guest_tsc: u64,
    pub requests: u64,
    /// Connections the front-door proxy routed to this VM.
    pub proxied: u64,
    /// Host time spent handling exits, by class: npf, mmio, io, msr, cpuid,
    /// hlt, intr, other (microseconds), and the matching counts.
    pub exit_host_us: [u64; 8],
    pub exit_count: [u64; 8],
    /// Nested page faults that allocated a fresh zero frame / only mapped
    /// an existing frame read-only.
    pub npf_zero: u64,
    pub npf_ro: u64,
    /// Write faults that only turned a clean private page dirty.
    pub npf_dirty: u64,
    /// Pages copied ahead of time from the snapshot's learned write set.
    pub eager_pages: u64,
    /// Port I/O exits by device: pic, pit, uart, other.
    pub io_class: [u64; 4],
    /// MMIO exits by device: lapic, vnet, other.
    pub mmio_class: [u64; 3],
    /// Interrupts delivered by source: timer, uart, net, other.
    pub inj: [u64; 4],
    /// Time runnable but not scheduled (microseconds).
    pub wait_us: u64,
    /// Frames sent to / received from the guest on its link.
    pub frames_to_guest: u64,
    pub frames_from_guest: u64,
    /// Pages not shared with the template (resident + swapped).
    pub private_pages: usize,
    /// Of those, pages inside the kernel text/rodata range (copy-on-write
    /// copies of kernel code: 0 means the kernel is fully shared).
    pub text_private_pages: usize,
    pub resident_pages: usize,
    pub swapped_pages: usize,
    pub npt_pages: usize,
    pub freezes: u64,
    pub thaws: u64,
    pub pages_written: u64,
    pub pages_loaded: u64,
    pub last_freeze_us: u64,
    pub last_thaw_us: u64,
    /// Time from request enqueue to the guest receiving it.
    pub last_wake_us: u64,
    pub wake_us_total: u64,
    pub wake_us_max: u64,
    pub wake_samples: u64,
    pub created_tsc: u64,
    pub last_active_tsc: u64,
    /// Time from creation to first hypercall / first console output.
    pub boot_us: u64,
    pub console_bytes: u64,
}

struct Ctl {
    state: VmState,
    queue: VecDeque<Request>,
    commands: VecDeque<Command>,
    stats: VmStats,
    crash_reason: Option<String>,
    logs: VecDeque<String>,
}

struct ConsoleState {
    attached: bool,
    line: Vec<u8>,
    input: VecDeque<u8>,
}

pub struct VmHandle {
    pub id: u32,
    pub name: String,
    pub kind: VmKind,
    pub mem_size: u64,
    pub service: Option<String>,
    ctl: SpinLock<Ctl>,
    console: SpinLock<ConsoleState>,
    /// Log every #VMEXIT (debugging aid).
    pub trace: AtomicBool,
    /// Wakes the vCPU task.
    pub notify: Notify,
    /// Set by devices (the network link) when the vCPU should run to
    /// deliver something; counts as work for the idle/frozen waits.
    pub extra_work: AtomicBool,
    /// The VM's network link, if it has one.
    pub link: SpinLock<Option<Arc<crate::net::vmlink::VmLink>>>,
    /// Snapshot this VM was cloned from, if any.
    pub origin: Option<String>,
    /// Installed image set the VM booted (inherited by clones).
    pub image: Option<String>,
    /// Result of the last `Command::Snapshot`.
    snapshot: SpinLock<Option<Result<Arc<super::image::Template>, String>>>,
    snapshot_notify: Notify,
    /// Baseline for `vm profile` deltas.
    prof_base: SpinLock<Option<VmStats>>,
    /// Activity marks by source (console, link, request, other).
    touches: [core::sync::atomic::AtomicU64; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    Timeout,
    VmDead,
    NotFound,
    Create(&'static str),
    NotAService,
}

impl core::fmt::Display for RequestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RequestError::Timeout => write!(f, "timed out"),
            RequestError::VmDead => write!(f, "vm is not running"),
            RequestError::NotFound => write!(f, "no such vm or service"),
            RequestError::Create(e) => write!(f, "could not create vm: {}", e),
            RequestError::NotAService => write!(f, "linux vms take console input, not requests (use 'vm attach')"),
        }
    }
}

impl VmHandle {
    pub fn new(id: u32, name: String, kind: VmKind, mem_size: u64, service: Option<String>, origin: Option<String>, image: Option<String>) -> Arc<VmHandle> {
        Arc::new(VmHandle {
            id,
            name,
            kind,
            mem_size,
            service,
            origin,
            image,
            snapshot: SpinLock::new(None),
            snapshot_notify: Notify::new(),
            prof_base: SpinLock::new(None),
            touches: [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)],
            ctl: SpinLock::new(Ctl {
                state: VmState::Created,
                queue: VecDeque::new(),
                commands: VecDeque::new(),
                stats: VmStats { created_tsc: time::now(), last_active_tsc: time::now(), ..Default::default() },
                crash_reason: None,
                logs: VecDeque::new(),
            }),
            console: SpinLock::new(ConsoleState { attached: false, line: Vec::new(), input: VecDeque::new() }),
            trace: AtomicBool::new(false),
            notify: Notify::new(),
            extra_work: AtomicBool::new(false),
            link: SpinLock::new(None),
        })
    }

    /// Remember the current counters as the baseline for `vm profile`.
    pub fn set_profile_base(&self) {
        let s = self.stats();
        *self.prof_base.lock() = Some(s);
    }

    pub fn profile_base(&self) -> Option<VmStats> {
        *self.prof_base.lock()
    }

    /// Consume the extra-work flag.
    pub fn take_extra_work(&self) -> bool {
        self.extra_work.swap(false, core::sync::atomic::Ordering::AcqRel)
    }

    pub fn link(&self) -> Option<Arc<crate::net::vmlink::VmLink>> {
        self.link.lock().clone()
    }

    pub fn state(&self) -> VmState {
        self.ctl.lock().state
    }

    pub fn set_state(&self, s: VmState) {
        self.ctl.lock().state = s;
    }

    pub fn stats(&self) -> VmStats {
        self.ctl.lock().stats
    }

    pub fn update_stats(&self, f: impl FnOnce(&mut VmStats)) {
        f(&mut self.ctl.lock().stats)
    }

    pub fn touch(&self) {
        self.touch_from(3);
    }

    /// Mark activity from a known source: 0 console, 1 link, 2 request, 3 other.
    pub fn touch_from(&self, src: usize) {
        self.touches[src.min(3)].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.ctl.lock().stats.last_active_tsc = time::now();
    }

    pub fn touch_counts(&self) -> [u64; 4] {
        let mut c = [0u64; 4];
        for (i, a) in self.touches.iter().enumerate() {
            c[i] = a.load(core::sync::atomic::Ordering::Relaxed);
        }
        c
    }

    /// Microseconds since the VM last did anything.
    pub fn idle_us(&self) -> u64 {
        let t = self.ctl.lock().stats.last_active_tsc;
        time::tsc_to_us(time::now().saturating_sub(t))
    }

    /// Queue a request; returns the new queue depth.
    pub fn enqueue(&self, req: Request) -> usize {
        let n = {
            let mut c = self.ctl.lock();
            c.queue.push_back(req);
            c.queue.len()
        };
        self.notify.notify_one();
        n
    }

    pub fn queue_len(&self) -> usize {
        self.ctl.lock().queue.len()
    }

    pub fn pop_request(&self) -> Option<Request> {
        self.ctl.lock().queue.pop_front()
    }

    /// Drop all queued requests (answering them with an error).
    pub fn drain_requests(&self, msg: &str) {
        let reqs: Vec<Request> = self.ctl.lock().queue.drain(..).collect();
        for r in reqs {
            if let Some(rep) = r.reply {
                rep.set(alloc::format!("error: {}", msg).into_bytes());
            }
        }
    }

    pub fn command(&self, cmd: Command) {
        self.ctl.lock().commands.push_back(cmd);
        self.notify.notify_one();
    }

    pub fn pop_command(&self) -> Option<Command> {
        self.ctl.lock().commands.pop_front()
    }

    /// Put a command back (without waking the vCPU) to retry it later.
    pub fn requeue_command(&self, cmd: Command) {
        self.ctl.lock().commands.push_back(cmd);
    }

    pub fn set_snapshot_result(&self, r: Result<Arc<super::image::Template>, String>) {
        *self.snapshot.lock() = Some(r);
        self.snapshot_notify.notify_one();
    }

    pub fn clear_snapshot_result(&self) {
        *self.snapshot.lock() = None;
        let _ = self.snapshot_notify.try_take();
    }

    /// Wait for the result of a `Command::Snapshot`.
    pub async fn wait_snapshot(&self, ms: u64) -> Option<Result<Arc<super::image::Template>, String>> {
        let deadline = time::now() + time::us_to_tsc(ms * 1000);
        loop {
            if let Some(r) = self.snapshot.lock().take() {
                return Some(r);
            }
            if time::now() >= deadline || self.is_finished() {
                return None;
            }
            let _ = crate::task::timer::timeout_until(deadline, self.snapshot_notify.notified()).await;
        }
    }

    pub fn has_command(&self) -> bool {
        !self.ctl.lock().commands.is_empty()
    }

    /// (requests queued, commands queued, console input bytes, extra_work).
    pub fn work_breakdown(&self) -> (usize, usize, usize, bool) {
        let c = self.ctl.lock();
        (c.queue.len(), c.commands.len(), self.console.lock().input.len(), self.extra_work.load(core::sync::atomic::Ordering::Acquire))
    }

    /// Anything for the vCPU to do besides waiting?
    pub fn has_work(&self) -> bool {
        let c = self.ctl.lock();
        !c.queue.is_empty()
            || !c.commands.is_empty()
            || !self.console.lock().input.is_empty()
            || self.extra_work.load(core::sync::atomic::Ordering::Acquire)
    }

    pub fn is_finished(&self) -> bool {
        self.state().is_finished()
    }

    pub fn crash_reason(&self) -> Option<String> {
        self.ctl.lock().crash_reason.clone()
    }

    pub fn set_crashed(&self, reason: String) {
        let mut c = self.ctl.lock();
        c.state = VmState::Crashed;
        c.crash_reason = Some(reason);
    }

    pub fn push_log(&self, line: String) {
        let mut c = self.ctl.lock();
        if c.logs.len() >= 64 {
            c.logs.pop_front();
        }
        c.logs.push_back(line);
    }

    pub fn logs(&self) -> Vec<String> {
        self.ctl.lock().logs.iter().cloned().collect()
    }

    // ------------------------------------------------------------ console --

    /// Host -> guest console bytes (Linux serial input).
    pub fn console_input(&self, bytes: &[u8]) {
        {
            let mut c = self.console.lock();
            for &b in bytes {
                if c.input.len() < 4096 {
                    c.input.push_back(b);
                }
            }
        }
        self.touch();
        self.notify.notify_one();
    }

    pub fn take_console_input(&self) -> Vec<u8> {
        self.console.lock().input.drain(..).collect()
    }

    pub fn has_console_input(&self) -> bool {
        !self.console.lock().input.is_empty()
    }

    pub fn set_attached(&self, on: bool) {
        let mut c = self.console.lock();
        c.attached = on;
        if !on && !c.line.is_empty() {
            let line = String::from_utf8_lossy(&c.line).into_owned();
            c.line.clear();
            drop(c);
            self.push_log(line);
        }
    }

    pub fn attached(&self) -> bool {
        self.console.lock().attached
    }

    /// Guest -> host console bytes.  Attached: written raw to the host
    /// console.  Detached: collected into lines and logged with a prefix.
    pub fn console_output(&self, bytes: &[u8]) {
        self.update_stats(|s| s.console_bytes += bytes.len() as u64);
        let attached = self.console.lock().attached;
        if attached {
            crate::console::write_bytes(bytes);
            // Keep the log ring current as well.
            let mut c = self.console.lock();
            for &b in bytes {
                if b == b'\n' {
                    let line = String::from_utf8_lossy(&c.line).into_owned();
                    c.line.clear();
                    drop(c);
                    self.push_log(line);
                    c = self.console.lock();
                } else if b != b'\r' && c.line.len() < 512 {
                    c.line.push(b);
                }
            }
            return;
        }
        let mut lines: Vec<String> = Vec::new();
        {
            let mut c = self.console.lock();
            for &b in bytes {
                if b == b'\n' {
                    lines.push(String::from_utf8_lossy(&c.line).into_owned());
                    c.line.clear();
                } else if b != b'\r' && c.line.len() < 512 {
                    c.line.push(b);
                }
            }
        }
        for l in lines {
            println!("[vm {} {}] {}", self.id, self.name, l);
            self.push_log(l);
        }
    }

    /// Send a request and wait for the answer.
    pub async fn request(&self, data: Vec<u8>, timeout_ms: u64) -> Result<Vec<u8>, RequestError> {
        if self.is_finished() {
            return Err(RequestError::VmDead);
        }
        if self.kind.is_linux() {
            return Err(RequestError::NotAService);
        }
        let reply = Reply::new();
        self.enqueue(Request { data, reply: Some(reply.clone()), enqueued: time::now() });
        match reply.wait(timeout_ms).await {
            Some(v) => Ok(v),
            None => {
                if self.is_finished() {
                    Err(RequestError::VmDead)
                } else {
                    Err(RequestError::Timeout)
                }
            }
        }
    }

    /// Wait until the VM reaches a finished state.
    pub async fn wait_finished(&self, timeout_ms: u64) -> bool {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        while !self.is_finished() {
            if time::now() >= deadline {
                return false;
            }
            timer::sleep_ms(2).await;
        }
        true
    }

    /// Wait until the VM is blocked (idle/frozen) or finished.
    pub async fn wait_blocked(&self, timeout_ms: u64) -> bool {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        loop {
            let s = self.state();
            if s.is_blocked() || s.is_finished() {
                return true;
            }
            if time::now() >= deadline {
                return false;
            }
            timer::sleep_ms(2).await;
        }
    }

    pub async fn wait_state(&self, want: VmState, timeout_ms: u64) -> bool {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        loop {
            let s = self.state();
            if s == want {
                return true;
            }
            if s.is_finished() || time::now() >= deadline {
                return false;
            }
            timer::sleep_ms(2).await;
        }
    }

    /// Wait until some console log line contains `needle`.
    pub async fn wait_console(&self, needle: &str, timeout_ms: u64) -> bool {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        loop {
            if self.logs().iter().any(|l| l.contains(needle)) {
                return true;
            }
            if self.is_finished() || time::now() >= deadline {
                return false;
            }
            timer::sleep_ms(20).await;
        }
    }
}
