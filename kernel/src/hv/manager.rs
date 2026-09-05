//! VM manager: registry, services with replicas, request routing, the
//! autoscaler that implements scale-to-zero policies, and the UDP front door.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::image::Template;
use super::vcpu::{vcpu_task, VmCore};
use super::vm::{Command, RequestError, VmHandle, VmKind, VmState};
use crate::sync::{OnceCell, SpinLock};
use crate::task::{self, timer};
use crate::time;

/// A scalable group of identical unikernel VMs.
#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub kind: u64,
    pub mem_size: u64,
    pub min_replicas: usize,
    pub max_replicas: usize,
    pub replicas: Vec<u32>,
    /// Freeze an idle replica after this many ms (0 = never).
    pub freeze_after_ms: u64,
    /// Destroy an idle replica after this many ms (0 = never).
    pub destroy_after_ms: u64,
    pub requests: u64,
    pub cold_starts: u64,
    pub next_replica: u32,
}

pub struct Manager {
    vms: SpinLock<BTreeMap<u32, Arc<VmHandle>>>,
    services: SpinLock<BTreeMap<String, Service>>,
    templates: SpinLock<BTreeMap<u64, Arc<Template>>>,
    /// Linux boot templates by (image set, command line, memory size).
    linux_templates: SpinLock<BTreeMap<(String, String, u64), Arc<Template>>>,
    /// Snapshot templates by name (`vm snapshot`).
    snapshots: SpinLock<BTreeMap<String, Arc<Template>>>,
    next_id: AtomicU32,
    pub created: AtomicU64,
    pub finished: AtomicU64,
    /// Freeze idle standalone VMs after this many ms (0 = never).
    pub default_freeze_ms: AtomicU64,
    /// Freeze quiet Linux VMs after this many ms of no console activity (0 = never).
    pub linux_freeze_ms: AtomicU64,
    pub udp_requests: AtomicU64,
    /// Newly created VMs start with exit tracing enabled.
    pub trace_new: core::sync::atomic::AtomicBool,
    /// Map all template pages eagerly when a Linux VM is created or thawed.
    pub prefault: core::sync::atomic::AtomicBool,
    /// Give clones private copies of the learned write set before they run.
    pub eager_cow: core::sync::atomic::AtomicBool,
    /// Read a frozen VM's swapped pages back in one batch when it thaws.
    pub prefetch: core::sync::atomic::AtomicBool,
}

static MANAGER: OnceCell<Manager> = OnceCell::new();

pub fn manager() -> &'static Manager {
    MANAGER.expect("vm manager")
}

pub fn init() {
    MANAGER.init(Manager {
        vms: SpinLock::new(BTreeMap::new()),
        services: SpinLock::new(BTreeMap::new()),
        templates: SpinLock::new(BTreeMap::new()),
        linux_templates: SpinLock::new(BTreeMap::new()),
        snapshots: SpinLock::new(BTreeMap::new()),
        next_id: AtomicU32::new(1),
        created: AtomicU64::new(0),
        finished: AtomicU64::new(0),
        default_freeze_ms: AtomicU64::new(0),
        linux_freeze_ms: AtomicU64::new(0),
        udp_requests: AtomicU64::new(0),
        trace_new: core::sync::atomic::AtomicBool::new(false),
        prefault: core::sync::atomic::AtomicBool::new(true),
        eager_cow: core::sync::atomic::AtomicBool::new(true),
        prefetch: core::sync::atomic::AtomicBool::new(true),
    });
    task::spawn_detached("hv-autoscaler", autoscaler_task());
    task::spawn_detached("hv-udp", udp_server_task());
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub vms: usize,
    pub running: usize,
    pub idle: usize,
    pub sleeping: usize,
    pub frozen: usize,
    pub linux: usize,
    pub resident_pages: usize,
    pub swapped_pages: usize,
    pub npt_pages: usize,
    pub services: usize,
}

impl Manager {
    pub fn template(&self, mem_size: u64) -> Result<Arc<Template>, &'static str> {
        if let Some(t) = self.templates.lock().get(&mem_size) {
            return Ok(t.clone());
        }
        let t = Template::build(mem_size)?;
        self.templates.lock().insert(mem_size, t.clone());
        Ok(t)
    }

    pub fn templates(&self) -> Vec<Arc<Template>> {
        let mut v: Vec<Arc<Template>> = self.templates.lock().values().cloned().collect();
        v.extend(self.linux_templates.lock().values().cloned());
        v.extend(self.snapshots.lock().values().cloned());
        v
    }

    fn check_name(&self, name: &str) -> Result<(), &'static str> {
        if name.is_empty() || name.contains(' ') {
            return Err("invalid vm name");
        }
        if self.vms.lock().values().any(|v| v.name == name) {
            return Err("a vm with that name exists");
        }
        Ok(())
    }

    /// Create the handle, core and task for a VM from a ready template.
    fn spawn_vm(
        &self,
        name: &str,
        kind: VmKind,
        template: Arc<Template>,
        service: Option<String>,
        origin: Option<String>,
        start_frozen: bool,
    ) -> Result<Arc<VmHandle>, &'static str> {
        if !super::is_enabled() {
            return Err("hypervisor not enabled on this CPU");
        }
        self.check_name(name)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handle = VmHandle::new(id, String::from(name), kind, template.mem_size, service, origin, template.image.clone());
        let trace = self.trace_new.load(Ordering::Relaxed);
        handle.trace.store(trace, Ordering::Relaxed);
        if trace {
            log!("vm {} {}: creating core ({} template pages)", id, name, template.pages.len());
        }
        if handle.kind.is_linux() {
            // A private point-to-point link; every guest is 10.42.0.2 on it.
            let link = crate::net::vmlink::VmLink::new(handle.clone());
            let iface = crate::net::add_interface(
                &format!("vm{}", id),
                link.clone(),
                crate::net::Ipv4Addr([10, 42, 0, 1]),
                crate::net::Ipv4Addr([255, 255, 255, 0]),
            );
            link.attach(&iface);
            *handle.link.lock() = Some(link);
        }
        let core = match VmCore::new(handle.clone(), template, start_frozen) {
            Ok(c) => c,
            Err(e) => {
                if let Some(link) = handle.link.lock().take() {
                    crate::net::remove_interface(link.iface_id());
                }
                return Err(e);
            }
        };
        if trace {
            log!("vm {} {}: core ready, spawning vcpu task", id, name);
        }
        self.vms.lock().insert(id, handle.clone());
        self.created.fetch_add(1, Ordering::Relaxed);
        task::spawn_detached("vcpu", vcpu_task(core));
        Ok(handle)
    }

    /// Create and start a unikernel VM.
    pub fn create_vm(&self, name: &str, kind: u64, mem_size: u64, service: Option<String>) -> Result<Arc<VmHandle>, &'static str> {
        if !super::is_enabled() {
            return Err("hypervisor not enabled on this CPU");
        }
        self.check_name(name)?;
        let template = self.template(mem_size)?;
        self.spawn_vm(name, VmKind::Unikernel(kind), template, service, None, false)
    }

    /// Snapshot a Linux VM into a named template.  The vCPU does the work at
    /// its next halt; this waits for the result.
    pub async fn snapshot_vm(&self, vm: &Arc<VmHandle>, name: &str) -> Result<Arc<Template>, String> {
        if name.is_empty() || name.contains(' ') {
            return Err(String::from("invalid snapshot name"));
        }
        if self.snapshots.lock().contains_key(name) {
            return Err(String::from("a snapshot with that name exists"));
        }
        if !vm.kind.is_linux() {
            return Err(String::from("only linux vms can be snapshotted"));
        }
        if vm.is_finished() {
            return Err(String::from("vm is not running"));
        }
        vm.clear_snapshot_result();
        vm.command(Command::Snapshot(String::from(name)));
        match vm.wait_snapshot(300_000).await {
            Some(Ok(t)) => {
                self.snapshots.lock().insert(String::from(name), t.clone());
                Ok(t)
            }
            Some(Err(e)) => Err(e),
            None => Err(String::from("snapshot timed out (vm never halted?)")),
        }
    }

    pub fn snapshot(&self, name: &str) -> Option<Arc<Template>> {
        self.snapshots.lock().get(name).cloned()
    }

    pub fn snapshots(&self) -> Vec<(String, Arc<Template>)> {
        self.snapshots.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Create a VM that resumes from a snapshot.  `frozen` creates it scaled
    /// to zero: it costs nothing until the first request thaws it.
    pub fn clone_vm(&self, snapshot: &str, name: &str, frozen: bool) -> Result<Arc<VmHandle>, &'static str> {
        let t = self.snapshot(snapshot).ok_or("no such snapshot")?;
        self.spawn_vm(name, VmKind::Linux, t, None, Some(String::from(snapshot)), frozen)
    }

    /// Create and start a Linux VM from the installed kernel image.
    /// Create and start a Linux VM.  `image` picks an installed image set
    /// (`None` = the first one); each set has its own kernel, initramfs and
    /// command line, so different VMs can run different kernels.
    pub async fn create_linux(&self, name: &str, mem_size: u64, cmdline_override: Option<&str>, image: Option<&str>) -> Result<Arc<VmHandle>, String> {
        if !super::is_enabled() {
            return Err("hypervisor not enabled on this CPU".into());
        }
        self.check_name(name).map_err(String::from)?;
        let dev = crate::disk::device().ok_or("no disk device for kernel images")?;
        crate::disk::images::wait_loaded(10_000).await;
        let set = match image {
            Some(i) => crate::disk::images::find_set(i).ok_or_else(|| {
                let have: Vec<String> = crate::disk::images::sets().into_iter().map(|s| s.name).collect();
                format!("no image '{}' installed (have: {})", i, if have.is_empty() { String::from("none") } else { have.join(", ") })
            })?,
            None => crate::disk::images::default_set().ok_or("no kernel image installed (run: cargo xtask install-linux)")?,
        };
        let (kernel, initrd) = (set.kernel.clone(), set.initrd.clone());
        let mut cmdline = match cmdline_override {
            Some(c) => String::from(c),
            None => match &set.cmdline {
                Some(e) => {
                    let v = crate::disk::images::read_image_vec(dev, e).await.map_err(|e| format!("disk read failed: {:?}", e))?;
                    String::from_utf8_lossy(&v).trim_end_matches(char::from(0)).trim().into()
                }
                None => String::from("console=ttyS0 rdinit=/init"),
            },
        };
        // The guest's TSC runs at the host rate; telling Linux the frequency
        // avoids relying on PIT calibration under emulation.
        if !cmdline.contains("tsc_early_khz=") {
            cmdline.push_str(&format!(" tsc_early_khz={}", time::tsc_per_ms()));
        }
        // The virtio-mmio network device has no ACPI/DT description.
        if !cmdline.contains("virtio_mmio.device=") {
            cmdline.push_str(&format!(
                " virtio_mmio.device=4K@{:#x}:{}",
                super::devices::vnet::VNET_BASE,
                super::devices::vnet::VNET_IRQ
            ));
        }
        // The image set is part of the key: two kernels can share a command
        // line and a memory size without sharing a template.
        let key = (set.name.clone(), cmdline.clone(), mem_size);
        // Bind the lookup first: a guard living in a `match` scrutinee would
        // stay locked across the await below.
        let cached = self.linux_templates.lock().get(&key).cloned();
        let template = match cached {
            Some(t) => t,
            None => {
                let t0 = time::now();
                let t = super::linux_boot::build_template(dev, &kernel, initrd.as_ref(), &cmdline, mem_size).await?;
                log!(
                    "linux: template built in {} ms ({} MiB guest, {} pages)",
                    time::tsc_to_us(time::now() - t0) / 1000,
                    mem_size >> 20,
                    t.pages.len()
                );
                self.linux_templates.lock().insert(key, t.clone());
                t
            }
        };
        self.spawn_vm(name, VmKind::Linux, template, None, None, false).map_err(String::from)
    }

    /// Called by the vCPU task when a VM reaches a final state.
    pub fn vm_finished(&self, h: &VmHandle) {
        self.finished.fetch_add(1, Ordering::Relaxed);
        self.vms.lock().remove(&h.id);
        if let Some(link) = h.link.lock().take() {
            crate::net::remove_interface(link.iface_id());
        }
        if let Some(svc) = &h.service {
            if let Some(s) = self.services.lock().get_mut(svc) {
                s.replicas.retain(|&id| id != h.id);
            }
        }
    }

    pub fn find(&self, name_or_id: &str) -> Option<Arc<VmHandle>> {
        let vms = self.vms.lock();
        if let Ok(id) = name_or_id.parse::<u32>() {
            if let Some(v) = vms.get(&id) {
                return Some(v.clone());
            }
        }
        vms.values().find(|v| v.name == name_or_id).cloned()
    }

    pub fn get(&self, id: u32) -> Option<Arc<VmHandle>> {
        self.vms.lock().get(&id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<VmHandle>> {
        self.vms.lock().values().cloned().collect()
    }

    pub fn count(&self) -> usize {
        self.vms.lock().len()
    }

    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for v in self.list() {
            s.vms += 1;
            if v.kind.is_linux() {
                s.linux += 1;
            }
            match v.state() {
                VmState::Running | VmState::Created => s.running += 1,
                VmState::Idle => s.idle += 1,
                VmState::Sleeping => s.sleeping += 1,
                VmState::Frozen => s.frozen += 1,
                _ => {}
            }
            let st = v.stats();
            s.resident_pages += st.resident_pages;
            s.swapped_pages += st.swapped_pages;
            s.npt_pages += st.npt_pages;
        }
        s.services = self.services.lock().len();
        s
    }

    pub fn kill_all(&self) -> usize {
        let vms = self.list();
        for v in &vms {
            v.command(Command::Kill);
        }
        vms.len()
    }

    /// Wait until no VMs remain (after `kill_all`).
    pub async fn wait_empty(&self, timeout_ms: u64) -> bool {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        while self.count() > 0 {
            if time::now() >= deadline {
                return false;
            }
            timer::sleep_ms(2).await;
        }
        true
    }

    // ------------------------------------------------------------ services --

    pub fn create_service(&self, name: &str, kind: u64, mem_size: u64, max_replicas: usize) -> Result<(), &'static str> {
        if name.is_empty() || name.contains(' ') {
            return Err("invalid service name");
        }
        let mut svcs = self.services.lock();
        if svcs.contains_key(name) {
            return Err("service exists");
        }
        svcs.insert(
            String::from(name),
            Service {
                name: String::from(name),
                kind,
                mem_size,
                min_replicas: 0,
                max_replicas: max_replicas.max(1),
                replicas: Vec::new(),
                freeze_after_ms: 2000,
                destroy_after_ms: 0,
                requests: 0,
                cold_starts: 0,
                next_replica: 1,
            },
        );
        Ok(())
    }

    pub fn delete_service(&self, name: &str) -> bool {
        let svc = self.services.lock().remove(name);
        match svc {
            Some(s) => {
                for id in s.replicas {
                    if let Some(v) = self.get(id) {
                        v.command(Command::Kill);
                    }
                }
                true
            }
            None => false,
        }
    }

    pub fn service(&self, name: &str) -> Option<Service> {
        self.services.lock().get(name).cloned()
    }

    pub fn services(&self) -> Vec<Service> {
        self.services.lock().values().cloned().collect()
    }

    pub fn set_service(&self, name: &str, f: impl FnOnce(&mut Service)) -> bool {
        match self.services.lock().get_mut(name) {
            Some(s) => {
                f(s);
                true
            }
            None => false,
        }
    }

    /// Live replicas of a service.
    pub fn replicas(&self, name: &str) -> Vec<Arc<VmHandle>> {
        let ids = match self.services.lock().get(name) {
            Some(s) => s.replicas.clone(),
            None => return Vec::new(),
        };
        let vms = self.vms.lock();
        ids.iter().filter_map(|id| vms.get(id).cloned()).filter(|v| !v.is_finished()).collect()
    }

    /// Choose (or create) a replica to serve one request.
    fn pick_replica(&self, name: &str) -> Result<Arc<VmHandle>, RequestError> {
        let (kind, mem_size, max, next) = {
            let mut svcs = self.services.lock();
            let s = svcs.get_mut(name).ok_or(RequestError::NotFound)?;
            s.requests += 1;
            (s.kind, s.mem_size, s.max_replicas, s.next_replica)
        };
        let replicas = self.replicas(name);

        // Prefer a warm idle replica, then a frozen one, then the shortest
        // queue; spawn if everyone is busy and there is room.
        let mut best: Option<(u32, Arc<VmHandle>)> = None;
        for v in &replicas {
            let score = match v.state() {
                VmState::Idle if v.queue_len() == 0 => 0,
                VmState::Frozen if v.queue_len() == 0 => 1,
                _ => 10 + v.queue_len() as u32,
            };
            if best.as_ref().map_or(true, |(b, _)| score < *b) {
                best = Some((score, v.clone()));
            }
        }
        if let Some((score, v)) = &best {
            if *score < 10 || replicas.len() >= max {
                return Ok(v.clone());
            }
        }
        // Scale out.
        let vm_name = format!("{}-{}", name, next);
        if let Some(s) = self.services.lock().get_mut(name) {
            s.next_replica += 1;
        }
        let h = self.create_vm(&vm_name, kind, mem_size, Some(String::from(name))).map_err(RequestError::Create)?;
        if let Some(s) = self.services.lock().get_mut(name) {
            s.replicas.push(h.id);
            s.cold_starts += 1;
        }
        Ok(h)
    }

    /// Send a request to a VM (by name or id) or a service.  Returns the
    /// reply and the end-to-end latency in microseconds.
    pub async fn request(&self, target: &str, data: Vec<u8>, timeout_ms: u64) -> Result<(Vec<u8>, u64), RequestError> {
        let t0 = time::now();
        let vm = match self.find(target) {
            Some(v) => v,
            None => self.pick_replica(target)?,
        };
        let reply = vm.request(data, timeout_ms).await?;
        Ok((reply, time::tsc_to_us(time::now() - t0)))
    }
}

/// Periodically apply idle policies: freeze warm-idle VMs, destroy replicas
/// that have been idle for long enough (down to zero).
async fn autoscaler_task() {
    loop {
        timer::sleep_ms(100).await;
        let m = manager();
        let services = m.services();
        let vms = m.list();
        let default_freeze = m.default_freeze_ms.load(Ordering::Relaxed);
        let linux_freeze = m.linux_freeze_ms.load(Ordering::Relaxed);
        for v in &vms {
            let (freeze_ms, destroy_ms, min) = match &v.service {
                Some(name) => match services.iter().find(|s| &s.name == name) {
                    Some(s) => (s.freeze_after_ms, s.destroy_after_ms, s.min_replicas),
                    None => (default_freeze, 0, 0),
                },
                None if v.kind.is_linux() => (linux_freeze, 0, 0),
                None => (default_freeze, 0, 0),
            };
            let st = v.state();
            if v.has_work() {
                continue;
            }
            let idle_us = v.idle_us();
            if destroy_ms > 0 && (st == VmState::Idle || st == VmState::Frozen) && idle_us > destroy_ms * 1000 {
                let alive = v.service.as_ref().map(|n| m.replicas(n).len()).unwrap_or(1);
                if alive > min {
                    v.command(Command::Kill);
                    continue;
                }
            }
            if freeze_ms > 0 && st == VmState::Idle && idle_us > freeze_ms * 1000 {
                v.command(Command::Freeze);
            }
        }
    }
}

/// UDP front door on port 7777: datagram `"<target> <payload>"` → reply.
async fn udp_server_task() {
    // Wait for the network to come up.
    let iface = loop {
        if let Some(i) = crate::net::interface() {
            break i;
        }
        timer::sleep_ms(500).await;
        if crate::virtio::net::device().is_none() {
            return;
        }
    };
    iface.wait_configured(30_000).await;
    let sock = match crate::net::udp::UdpSocket::bind(7777) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log!("hv: udp server bind failed: {}", e);
            return;
        }
    };
    log!("hv: request server listening on udp port 7777 (\"<vm|service> <payload>\")");
    loop {
        let d = sock.recv().await;
        manager().udp_requests.fetch_add(1, Ordering::Relaxed);
        let sock2 = sock.clone();
        task::spawn_detached("hv-udp-req", async move {
            let text = String::from_utf8_lossy(&d.data).into_owned();
            let text = text.trim_end();
            let (target, payload) = match text.find(' ') {
                Some(i) => (&text[..i], text[i + 1..].as_bytes()),
                None => (text, &b""[..]),
            };
            let reply = match manager().request(target, payload.to_vec(), 10_000).await {
                Ok((r, us)) => {
                    let mut out = r;
                    out.extend_from_slice(format!(" [{} us]\n", us).as_bytes());
                    out
                }
                Err(e) => format!("error: {}\n", e).into_bytes(),
            };
            sock2.send_to(&reply, d.src, d.src_port).await;
        });
    }
}
