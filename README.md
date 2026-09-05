# conc_os

An OS where VMs are first-class citizens, i.e. a type-1 hypervisor that supports scale-to-zero and other operations.

conc_os is written from scratch in Rust. It boots via UEFI, owns the machine, and runs
guests with AMD-V (SVM) plus nested paging. Every virtual CPU is an async task; a VM that is
waiting for work is simply never polled, and after a configurable idle period its memory is
evicted to disk so an idle VM costs neither CPU nor RAM. Two kinds of guest run today: the
bundled request/response unikernel, and unmodified Linux kernels booted as microVMs with a
small legacy-PC device model. It runs under QEMU's TCG (which emulates SVM, so no
nested-virtualisation support is needed on the host) and is designed to run on real AMD
hardware.

## Features + Roadmap

- [x] Memory allocation
- [x] Async
- [x] Networking
- [x] Disk
- [x] Launching single VM
- [x] Launching multiple VMs
- [x] Scaling and scheduling VMs
- [x] Scale-to-zero
- [x] Fast scale-to-zero
- [x] Scale-to-zero for large number of VMs
- [x] Large number of VMs with low latency & scale to zero
- [x] Linux microVMs (unmodified Linux kernel + initramfs, serial console, scale-to-zero)
- [x] Web front door: TCP (smoltcp), virtio-net for guests, an SNI/Host-routing proxy that thaws the target VM
- [x] Snapshots and clones: resume a booted Linux VM's memory + CPU + device state; one shared kernel image, copy-on-write
- [x] 1000 Linux microVMs with ~10 active at once, measured (`web-test 1000 10`)

## Quick start

Requirements: a Rust nightly toolchain (`rust-toolchain.toml` pins it and pulls the
`x86_64-unknown-uefi` and `x86_64-unknown-none` targets), and QEMU with its bundled edk2
firmware (`qemu-system-x86_64` on `PATH`; on Windows the default install location is also
searched).

```bash
cargo xtask run
```

builds the guest image and the kernel, drops the kernel into `esp/EFI/BOOT/BOOTX64.EFI`,
creates a 512 MiB `disk.img`, boots QEMU headless and bridges the serial console to your
terminal. Type `help` at the `conc_os>` prompt. Useful options: `--gui` (show the QEMU
window), `--mem 4G`, `--cmd "<shell command>"` (scripted input, repeatable), `--timeout <s>`.

```bash
cargo xtask test
```

boots, runs the built-in self tests (`selftest` in the shell) and exits with a status code.

### A tour

```text
conc_os> vm create web echo             # a VM running the "echo" service
conc_os> req web hello world            # send it a request, get the reply + latency
echo[11]: HELLO WORLD (fnv1a=...) (412 us)
conc_os> svc create calc primes 4       # an auto-scaling service, up to 4 replicas
conc_os> svc set calc freeze_ms=500 destroy_ms=5000
conc_os> bench calc 200 8               # 200 requests, 8 in flight
conc_os> vms                            # per-VM state, resident/swapped pages, cpu time
conc_os> vm freeze web                  # evict its memory to disk (state kept)
conc_os> req web still there?           # transparently thawed on demand
conc_os> scale-test 1000                # the full scale-to-zero scenario, with numbers
conc_os> hv                             # hypervisor + memory summary
```

Requests can also come from outside: QEMU forwards host UDP port 7777 into the OS, where a
request server answers datagrams of the form `<vm-or-service> <payload>`:

```powershell
$u = New-Object System.Net.Sockets.UdpClient; $u.Connect("127.0.0.1", 7777)
$b = [Text.Encoding]::ASCII.GetBytes("calc 100000"); $u.Send($b, $b.Length) | Out-Null
$ep = New-Object System.Net.IPEndPoint([Net.IPAddress]::Any, 0)
[Text.Encoding]::ASCII.GetString($u.Receive([ref]$ep))     # primes<=100000: 9592 (2159 us) [4489 us]
```

### Linux microVMs

conc_os boots stock Linux kernels as VMs. Install a kernel and a busybox initramfs into the
image area of `disk.img` once:

```bash
cargo xtask install-linux --kernel images/vmlinux --busybox images/busybox
```

`--kernel` accepts an uncompressed `vmlinux` ELF or a `bzImage` (64-bit boot protocol);
`--initrd <cpio>` supplies your own initramfs instead of the generated busybox one,
`--init <script>` replaces just the `/init` of the generated one, and `--cmdline "..."`
overrides the kernel command line. Then, in the shell:

```text
conc_os> linux create lx 128            # 128 MiB Linux VM from the installed image
conc_os> vm attach lx                   # its serial console; Ctrl-] detaches
[    0.000000] Linux version 5.10.233 ...
...
conc_os linux guest: Linux 5.10.233 booted; MemTotal: 111380 kB
/ # cat /proc/cpuinfo
/ # poweroff -f                         # halts the guest; conc_os retires the VM
conc_os> vm freeze lx                   # or: hv set linux_freeze_ms=2000
conc_os> vm send lx uptime              # console input thaws the VM again
```

A quiet Linux VM parks in `hlt` between timer interrupts and can be frozen to disk like any
other VM; the next console input thaws it. Verified with the Firecracker CI kernel
`vmlinux-5.10.233-no-acpi` (fetched from the public Firecracker S3 bucket) and Ubuntu's
static busybox: the kernel reaches a shell in about 4 s of guest time, runs commands typed
into its console, and `poweroff -f` ends the VM. The self tests `hv::linux_boot_shell` and
`hv::linux_freeze_thaw` cover boot, console I/O, idle CPU use, power-off, and freezing a
booted Linux VM to disk and thawing it on input. `hv::linux_network`, `hv::linux_proxy` and
`hv::linux_snapshot_clone` cover the virtio-net link, routing by Host header and SNI (a
frozen VM answering with its counter intact), and snapshot/clone (independent counters,
per-clone VM ids, almost no private memory). The whole 34-test suite, five Linux boots
included, runs in about 45 s on the WSL QEMU described below.

**Windows hosts:** the QEMU builds for Windows compute SVM segment-base canonicalisation
with a 32-bit `long` (fixed upstream after 10.0), which truncates every guest GDT/IDT/FS/GS
base above 32 MiB on `VMRUN`. Unikernel guests keep all bases low and are unaffected, but
Linux cannot survive it. On Windows, run Linux guests through a Linux QEMU inside WSL: run
`tools/wsl-qemu.sh` once inside WSL (it unpacks Ubuntu's `qemu-system-x86` into
`~/.conc_os/qemu` without root), then use `cargo xtask run --wsl` / `cargo xtask test --wsl`.

### Heterogeneous guests: several kernels side by side

Installed images are *named sets* of a kernel, an initramfs and a command line, and
`install-linux` keeps the sets it is not writing, so different VMs can run different
kernels at the same time:

```bash
cargo xtask install-linux --name linux510 --kernel images/vmlinux-5.10.233-no-acpi
cargo xtask install-linux --name linux61  --kernel images/vmlinux-6.1.168
cargo xtask install-linux --name static   --kernel images/vmlinux-5.10.252-no-acpi \
                          --init tools/init-static-web.sh
cargo xtask install-linux --remove static          # frees its space again
```

```text
conc_os> linux images
image              kernel  initramfs      total   default
linux510         36.0 MiB    7.1 MiB   43.1 MiB   yes
linux61          42.5 MiB    7.1 MiB   49.6 MiB
static           37.2 MiB    7.1 MiB   44.3 MiB
conc_os> linux create go 128                    # the default (first) image
conc_os> linux create --image static st 128     # a different kernel and a different app
conc_os> linux create --image linux61 six 128
conc_os> vm http go /
HTTP 200 in 98.601 ms: vm=1 host=conc-linux hits=1 uptime=85.5s proto=http sni=go path=/
conc_os> vm http st /
HTTP 200 in 71.474 ms: static page from conc-static on Linux 5.10.252+
```

The first installed set is the default; `--image` picks another, and the choice is part of
the boot-template cache key, so two kernels never share a template. `vm info` names the
image a VM booted, and snapshots and their clones inherit it. Nothing else in the system
cares which kernel a VM runs: the front door routes to `go`, `st` and `six` by name the
same way, and each guest is `10.42.0.2` on its own link.

That example is the tested one: Linux 5.10.233, 5.10.252 and 6.1.168 booting concurrently,
two of them serving different web applications (the Go request counter and a busybox
`httpd` serving a static page). They do not even take the same path through the device
model — 5.10 has `CONFIG_X86_MPPARSE` and uses the MP table and the I/O APIC, while the
6.1 build does not and falls back to virtual-wire PIC mode. The self test
`hv::linux_image_sets` boots one VM from each of the first two installed sets and checks
they run different kernels; it skips when only one set is installed.

One caveat found while testing: the prebuilt 6.1.168 kernel is built without
`CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`, so it ignores `virtio_mmio.device=` and comes up
without a NIC (it boots to a shell and runs its userspace, but nothing can reach it). That
kernel expects ACPI tables to describe the device, and conc_os provides an MP table but no
ACPI. Kernels that can take a virtio-mmio device from the command line — like Firecracker's
`-no-acpi` builds — get networking with no configuration at all.

**Hardware virtualization:** on an AMD host with nested virtualization enabled for WSL2
(`/dev/kvm` exists inside WSL), `cargo xtask run --kvm` / `test --kvm` runs the same Linux
QEMU with `-accel kvm -cpu host`, so conc_os executes its `VMRUN`s on the real CPU (nested
under KVM under Hyper-V) instead of being emulated. `/dev/kvm` is group-restricted in WSL, so
`--kvm` starts only the QEMU process as root inside the distro (`wsl -u root`); add your
user to the `kvm` group if you prefer. Guest code then runs at native speed, but every
`#VMEXIT` is reflected through three hypervisors and costs about 200 µs — which is exactly
what the profiling section below is about.

### Web front door, snapshots and clones

Every Linux VM gets a virtio-mmio network device on a private point-to-point link to the
host stack; the guest is always `10.42.0.2`, the host `10.42.0.1`. The initramfs can carry
`webcounter`, a small Go server (`guest-web/`) that answers on HTTPS :443 (self-signed
`*.conc` certificate) and HTTP :80 with its VM id (from a hypercall), hostname and the
number of requests it has served since boot. Build it once under WSL/Linux and reinstall:

```bash
bash tools/build-webcounter.sh          # Go toolchain in ~/.conc_os/go or PATH -> images/webcounter
cargo xtask install-linux               # picks images/webcounter up automatically
```

conc_os listens on ports 443 and 80 of its own interface (QEMU forwards host 8443 and 8080
to them). A TLS connection is routed by the SNI in its ClientHello, a plain HTTP one by its
Host header; the first DNS label is the VM name. Routing to a frozen VM thaws it: the guest
never notices it was gone. TLS is passed through untouched, so from the host:

```bash
curl -k --resolve vm0001.conc:8443:127.0.0.1 https://vm0001.conc:8443/
# vm=1 host=conc-linux hits=1 uptime=0.5s proto=https sni=vm0001.conc path=/
```

Booting takes seconds; cloning a snapshot takes microseconds:

```text
conc_os> linux create base 128             # boot once (about 5 s until the server is up)
conc_os> vm snapshot base web              # 47 MiB of pages become a template, zero-copy, ~10 ms
conc_os> linux clone web vm 1000           # vm0001..vm1000, born frozen: 14 KiB each until used
conc_os> vm http vm0042 /                  # first request thaws it: about 90 ms
conc_os> web-test 1000 10                  # the whole experiment with a report (see below)
```

A snapshot is taken while the guest is halted: the VM's private frames simply become the
new template's frames (the VM continues on top of it, copy-on-write), and the VMCB,
registers, FPU state, guest TSC and the entire device model are captured. Clones resume from
that point sharing every page — including the whole kernel text — until they write to it.

## Architecture

```text
 UEFI firmware ─► efi_main ─► exit boot services ─► own page tables + stack ─► kernel_main
                                                                                   │
   ┌──────────────┬──────────────┬───────────────┬──────────────┬───────────────────┘
   ▼              ▼              ▼               ▼              ▼
 memory        interrupts     async executor   devices        hypervisor
 frame bitmap  GDT/IDT/TSS    tasks, wakers    PCI            SVM enable, VMCB, VMRUN
 slab heap     LAPIC timer    timers, channels virtio-net     nested page tables (COW)
 4-level       IOAPIC         notify/waitq     virtio-blk     guest templates, lazy paging
 paging        MSI-X          join_all         net stack      vCPU tasks, hypercalls
                                               page store     Linux device model + loader
                                               image area     manager, services, autoscaler
```

* **Boot** (`kernel/src/uefi.rs`, `main.rs`): a hand-written minimal UEFI FFI collects the
  memory map, framebuffer and ACPI pointer, then `ExitBootServices`. An early bump allocator
  builds an identity map of physical memory with 2 MiB pages (RAM write-back, everything else
  uncacheable) and a 1 MiB kernel stack; we switch to them and never look back.
* **Memory** (`mm/`): a bitmap frame allocator with an O(1) LIFO cache for single frames and
  a scan path for contiguous ranges; a size-class slab heap (no headers, O(1) free) with
  page-multiple allocations served straight from contiguous frames; a reusable 4-level page
  table mapper that also backs the nested page tables.
* **Interrupts and time** (`arch/`): 256 generated interrupt stubs, exceptions with register
  dumps, the local APIC timer calibrated against the PIT, an I/O APIC for the serial port,
  and MSI-X for virtio devices. Uptime is TSC-based.
* **Async** (`task/`): a single-threaded executor with interrupt-safe wakers; the idle loop
  is `sti; hlt`. Timers live in an ordered map driven by the tick; there are unbounded
  channels, a permit-carrying `Notify`, a broadcast `WaitQueue`, `join_all` and `timeout`.
  Everything long-lived — shell, network RX, disk init, autoscaler, every vCPU — is a task.
* **Networking** (`pci.rs`, `virtio/`, `net/`): PCI enumeration, a legacy virtio-PCI
  transport with MSI-X, virtio-net, and a stack with Ethernet, ARP (with async resolution),
  IPv4, ICMP echo (both ways), UDP sockets and a DHCP client. The self tests get a lease from
  QEMU, ping the gateway and complete a DNS round trip.
* **Disk** (`virtio/blk.rs`, `disk/`): an async virtio-blk driver with unique request
  tokens, backpressure when the ring is full and concurrent requests; a 4 KiB page store on
  top of it that holds the memory of frozen VMs; and an image area (upper half of the disk)
  that holds installed kernels, initramfs archives and command lines.
* **Hypervisor** (`hv/`): described below.

### The hypervisor

Guests are launched directly in 64-bit long mode. A *template* (`hv/image.rs`) is the
prepared initial memory of a VM plus its boot register state: identity-mapping page tables
(with accessed/dirty bits pre-set so the CPU never writes to them), a GDT, and the loaded
image — ten 4 KiB frames for the bundled unikernel, a few thousand for a Linux kernel and
initramfs. Every VM created from a template shares those frames copy-on-write.

Per-VM memory (`hv/memory.rs`) is a vector of page states — `Zero`, `Template`, `Private`,
or `Swapped` — plus nested page tables that start empty. Nested page faults populate them
lazily: reads of untouched pages map a shared zero frame or the template frame read-only;
the first write copies. A freshly created VM therefore owns a VMCB, one NPT frame and its
page-state vector, and nothing else until the guest runs.

The vCPU (`hv/vcpu.rs`) is an async task around `VMRUN`. For unikernel guests it handles
hypercalls (`vmmcall`), CPUID (with a hypervisor leaf), I/O (port 0x3F8 becomes a log line),
MSRs, nested page faults, and every guest exception, which is reported and kills only that
VM. Physical interrupts are intercepted, so the host timer preempts guests; a VM yields to
the executor after a 2 ms quantum, which gives round-robin scheduling among runnable VMs and
keeps the host responsive under CPU-bound guests (tested with two spinning guests).

Guests talk to the host through hypercalls: `log`, `wait_request`, `respond`, `exit`,
`uptime`, `yield`, `sleep`. The bundled guest (`guest/`) is a freestanding Rust program that
implements several request/response services selected at boot — `echo`, `primes` (a sieve),
`counter` (stateful), `sleepy`, plus `spin`, `fault` and `hello` for testing.

### Linux guests

Linux VMs get a Firecracker-style boot (`hv/linux_boot.rs`): the kernel's ELF segments are
copied to their physical addresses (or a bzImage to its preferred address and entered at
+0x200), the initramfs goes to the top of RAM, and a zero page at 0x7000 carries the
setup_header fields, the e820 map, the command line pointer and the initrd location. The
guest starts at `startup_64` with `rsi` pointing at the zero page, identity page tables at
0x9000 and a GDT at 0x500. conc_os appends `tsc_early_khz=<host TSC rate>` so the kernel
knows its clock without depending on calibration.

There is no ACPI, but the boot template carries an Intel MP table (one processor, an ISA
bus, an I/O APIC), so Linux runs in symmetric I/O mode: fixed vectors through the I/O
APIC, one local-APIC EOI per interrupt, and the local APIC timer in TSC-deadline mode as
its tick source. The legacy path still exists for kernels without `CONFIG_X86_MPPARSE`.
The device model (`hv/devices/`):

* a local APIC at 0xFEE00000, emulated through nested page faults plus an x86 MOV decoder
  (`hv/x86.rs`) that walks the guest's own page tables to fetch the faulting instruction
  (decodes are cached per RIP);
* an I/O APIC at 0xFEC00000 (24 inputs, edge and level, EOI register) that the PIT, the
  UART and the virtio device drive; two cascaded 8259 PICs remain wired in parallel for
  legacy mode, with their INTR output on LINT0 as ExtINT;
* an 8254 PIT (channel 2 serves TSC calibration; channel 0 is only the tick source in legacy
  mode);
* a 16550A UART at 0x3F8 on IRQ 4 for the console, with a THRE latch modelled the way the
  Linux driver's start-up test expects;
* a BCD CMOS RTC, the i8042 status stub that lets `reboot=k` reset the VM, and PCI config
  stubs that report no bus;
* a virtio-mmio network device (`hv/devices/vnet.rs`, `vq.rs`) at 0xD0000000 on IRQ 5,
  declared to the kernel with `virtio_mmio.device=4K@0xd0000000:5` since there is no device
  tree. It is a modern (version 2) transport with split virtqueues that the vCPU task
  services in place: a transmit kick runs the host network stack for the frame right there,
  and frames for the guest are copied into its receive ring before the next `VMRUN`.

Every device line goes to the 8259 the moment it changes, not at the next poll: the PIC is
edge-triggered, and an interrupt acknowledge followed by a new frame within one exit would
otherwise be a lost edge that silences the device for good (this was a real bug found by
the 1000-VM test).

Interrupts are injected with `EVENTINJ` only when the guest can take them (RFLAGS.IF set, no
interrupt shadow); otherwise a dummy `V_IRQ` opens an interrupt window and the `VINTR` exit
re-runs the decision. An event interrupted by an exit (`EXITINTINFO`) is re-injected. CPUID
is filtered to a plain single-core CPU: no XSAVE/AVX, MTRR, MCE, PCID, x2APIC, monitor or
TSC-deadline, so the set of MSRs the kernel touches stays small. `HLT` with interrupts
enabled parks the vCPU task until the next device deadline or console input; `HLT` with
interrupts disabled is `stop_this_cpu()` and retires the VM. Triple faults and the keyboard
controller reset pulse reboot the VM from its template.

Time inside a Linux guest is "time executing the guest": the hypervisor accumulates the
duration of every exit and every descheduled interval into the VMCB `TSC_OFFSET`, so the
guest's TSC, PIT and calibration loops see a consistent clock even though each exit costs
tens of microseconds under emulation. Halted waits let wall time flow so timer deadlines
arrive.

### Scale to zero

`wait_request` is the heart of it for unikernels. When a guest asks for its next request and
none is queued, the hypervisor leaves the guest's RIP on the `vmmcall` instruction and
returns `Block`; the vCPU task then awaits a notification. Nothing polls it: an idle VM costs
zero CPU. When a request arrives the task is woken, `VMRUN` resumes the guest, it re-executes
the hypercall and finds its request. Wake latency is a task wakeup plus one VM entry. For
Linux the equivalent is the idle `hlt`: the task sleeps until the PIT deadline or console
input.

There are three levels, each applied by the autoscaler (`hv/manager.rs`) after a
configurable idle time:

1. **Idle** — blocked in `wait_request` or `hlt`; zero CPU, memory warm.
2. **Frozen** — private pages written to the disk page store, frames freed, nested page
   tables destroyed. Only the VMCB, the page-state vector and the handle remain
   (about 13 KiB for a unikernel). Pages that were loaded from disk and not written since are
   dropped without any I/O; dirty pages are written in concurrent batches. Thawing allocates
   one NPT root and returns; pages come back on demand through nested page faults, so a wake
   touches only the pages the guest actually uses. For Linux, only console and network
   activity counts as activity (timer wakeups do not), so a quiet guest can be frozen with
   `hv set linux_freeze_ms=<ms>` and its clock simply jumps forward when it is thawed. A
   frame arriving on a frozen VM's link thaws it, which is how the front door works.
3. **Destroyed** — a service replica idle for long enough is deleted; the next request to the
   service cold-starts a new one from the template.

Services (`svc create`) group replicas: requests go to a warm idle replica, then a frozen
one, otherwise the shortest queue, and a new replica is created if all are busy and the
maximum has not been reached.

### Networking and the front door

The host stack (`net/`) is our own Ethernet/ARP/IPv4/ICMP/UDP/DHCP, with one `Interface`
per device. The primary interface sits on the virtio-net card; every Linux VM adds a
`vmlink` interface: a point-to-point link whose other end is the guest's virtio-mmio
device. Because the interface, not the address, identifies the peer, every guest can be
`10.42.0.2` and the same snapshot can be cloned a thousand times without renumbering.

TCP is smoltcp (`net/tcp.rs`), one engine per interface in `Medium::Ip` mode: our stack
hands it only IPv4 packets carrying TCP, and the complete packets it emits go out through
the interface's ARP cache. Engines are created lazily, so a thousand idle links cost
nothing; sockets get 64 KiB buffers each way, Reno congestion control, no delayed ACK and
no Nagle. smoltcp is polled under the engine lock on every incoming segment, after every
socket operation and from a 10 ms ticker for its timers; its wakers make `TcpStream` and
`TcpListener` ordinary async types. `net/http.rs` is a tiny HTTP/1.0 client on top.

The front door (`net/proxy.rs`) listens on 443 and 80. It reads just enough of a new
connection to find a name — the `server_name` extension of a TLS ClientHello, or the Host
header of a plain request — takes the first DNS label as the VM name, opens a TCP
connection to `10.42.0.2` over that VM's link, replays the bytes it sniffed and splices
both directions until either side closes. TLS is never terminated: the guest holds the
certificate. If the VM is frozen, the SYN itself thaws it; the connect simply takes a little
longer and the proxy records it as a cold route.

### Snapshots, clones and the shared kernel

Booting Linux to a listening web server takes about five seconds of emulated time; that is
paid once. `vm snapshot <vm> <name>` runs on the vCPU task at the guest's next halt (the
only point where the CPU state is clean: nothing pending, `RIP` after the `hlt`). It moves
every private frame of the VM into a new template (no copy — the VM continues on top of the
snapshot copy-on-write, exactly like a freshly created VM), copies the VMCB page, the
general-purpose registers, the FXSAVE area, `TSC_AUX`, the guest-visible TSC and the whole
device model (PIC, PIT, LAPIC, UART, RTC and the virtio queues), and records where the
kernel's text/rodata live. Templates are immutable after that; the base VM's own later
writes go to new frames.

`linux clone <snapshot> <name> [count]` creates VMs that resume from that point. A clone's
VMCB is the snapshot's page with its own nested page table root, ASID, permission maps and
a zeroed clean-bits field; its `TSC_OFFSET` is set so the guest clock continues from the
snapshot value; the device model is cloned and its virtio device re-pointed at the clone's
own link. Clones are born **frozen**: no nested page tables, no private pages, just the
handle, the VMCB and the sparse page-state overlay — 14 KiB of host memory each. The first
frame on the link thaws the clone, `prefault` maps every template page read-only in one
host-side pass (thousands of read faults saved; `hv set prefault=0` to compare), and the
guest resumes in the idle loop with the interrupt for the new packet pending.

That is also how the kernel is shared: the snapshot holds one copy of the kernel image in
host memory and every clone maps it at the same guest-physical address, read-only. A clone
gets its own frame for a page only when it writes to it. The per-VM page state is a sparse
overlay over the template (only private or swapped pages have entries), so freezing a
clone costs a few hundred disk blocks and its memory footprint stays proportional to what
it dirtied. The report prints how many pages of the kernel text range each clone has
copied: a few dozen (jump-label patching and the like), out of about four thousand.

### Profiling cold starts and warm requests

`vm coldstart <clone> [n]` freezes a VM, sends one request through the front door, then a
second one, and prints for each a timeline (client connect, proxy accept, sniff, connect to
the guest, first byte, close) and the VM's exit profile: exits by class with the host time
each class cost, time inside `VMRUN`, time runnable but descheduled, interrupts injected by
source, frames, pages faulted/copied/loaded. `vm profile <vm> [reset]` gives the same
counters between two points, per proxied request. The numbers that drove the work:

* Under TCG a warm request was 262 exits but only 1.2 ms of host handling against 16 ms of
  emulated guest execution: the emulator dominates and the hypervisor is a rounding error.
* Under nested KVM the same request was 125–161 exits at roughly **200 µs per exit round
  trip** (Hyper-V → KVM → conc_os → guest and back), so the exit count *was* the latency:
  a cold start of 1089 exits took 390 ms, a warm request 35–55 ms.

What each exit was, and what replaced it:

| Source of exits (per warm request) | Before | Fix | After |
|---|---|---|---|
| 8259 PIC: mask, EOI, unmask per interrupt (+ port 0x80 delays) | ~50 + 49 | MP table + I/O APIC: fixed vectors, one LAPIC EOI per interrupt; `io_delay=none` | 0 |
| 8254 PIT reprogramming per tick | ~20 | LAPIC timer in TSC-deadline mode (CPUID bit exposed, MSR 0x6E0): one `WRMSR` | 0 (MSR ~10) |
| `WRMSR FS/GS/KERNEL_GS_BASE` on context switches | 10–30 | MSRPM pass-through for everything `VMLOAD`/`VMSAVE` carries | 0 |
| Our own 1 kHz host tick interrupting the guest | 22 | Tickless host timer (one-shot APIC timer armed for the next deadline, 10 ms cap) | 3–9 |
| Host `VMSAVE` before every `VMRUN` (an intercepted instruction when nested) | 1 per entry | Saved once at init | 0 |
| virtio TX-completion interrupts the driver did not ask for | ~4 | Honour `VRING_AVAIL_F_NO_INTERRUPT` | 0 |
| LAPIC/virtio MMIO decode walking guest page tables | host time | Per-RIP decode cache | — |

Cold starts had three more:

| Cold-start cost | Before | Fix | After |
|---|---|---|---|
| Copy-on-write faults on a fresh clone's first request | 207–274 faults, 20 ms host + 50 ms exits | **Learned write set**: pages a clone dirties are recorded when it freezes; new clones get private copies of them before their first `VMRUN` | 0 faults |
| Re-thaw: one disk read per touched page, one fault each | 233 faults, 180 ms | Batched prefetch of the VM's swapped pages at thaw (one virtio queue of reads), mapped writable | 0 faults, 6–7 ms of reads |
| Read faults on shared template pages | ~130 | Prefault all template pages read-only in one host pass | 0 |

The learned set is per snapshot: every freeze of a clone records the pages it dirtied, and
pages reported by at least two clones (capped at 4096) are copied for newcomers, so a page
one clone happened to touch is not paid for by all of them. It is what makes "first request
on a brand-new clone" cost the same as a re-thaw.

**Result under nested KVM**: a fresh clone's first request 156 ms → **37 ms** (95 exits,
no page faults); a re-thaw 380–550 ms → **36 ms**; a warm request 35–55 ms → **14 ms**
(51 exits). Of the 51 remaining exits, 16 are virtio-mmio register accesses (notify,
ISR read and acknowledge: inherent to the MMIO transport), 13 LAPIC EOIs, 12 TSC-deadline
timer arms, 9 host interrupts and 2 `HLT`s. Firecracker on a non-nested KVM host serves a
warm request in about a millisecond and restores a snapshot in a few; at ~1–2 µs per
non-nested exit, the 51-exit budget above would cost ~100 µs plus guest work, i.e. the
same order — the remaining gap on this machine is the nesting, not the design. Under TCG
the same changes cut a cold start from 90 ms to about 35 ms and a warm request from 46 ms
to about 12 ms, with guest emulation now almost all of what is left.

Two measurement traps worth recording: every serial byte the profiler prints is an exit
when running nested (so it collects both phases before printing), and xtask's `@delay:`
commands are timed from when the previous command was *sent*, so a long-running command
makes later probes execute back-to-back — use the shell's blocking `sleep <ms>` instead.

### Measured (QEMU 10, TCG, one CPU, 1000 VMs of 512 KiB, `scale-test 1000`)

| Step                                             | Result                                          |
| ------------------------------------------------ | ----------------------------------------------- |
| Create 1000 VMs                                  | 364 ms total (364 µs per VM)                    |
| Warm memory footprint                            | 7 private pages per VM, 54 MiB total            |
| Warm wake latency (request → guest receives it)  | p50 35 µs, p99 151 µs, max 485 µs               |
| Freeze all 1000                                  | 676 ms total; 0 resident pages afterwards       |
| Frozen footprint                                 | 13 KiB per VM (12.9 MiB total)                  |
| Request latency from frozen (lazy restore)       | p50 565 µs, p99 1.3 ms, max 3.2 ms              |
| State preserved across freeze/thaw               | 1000 of 1000                                    |
| Destroy all 1000                                 | 20 ms                                           |

These are emulator numbers; TCG makes every VM entry/exit far slower than hardware, so real
AMD machines should be well below them.

Driven from the Windows host over UDP (`svc create calc primes 4`, freeze after 500 ms,
destroy after 8 s): single requests answered in 0.3–5 ms round trip, a burst of 20 requests
scaled the service out to 4 replicas and completed in 108 ms, and after the idle timeout the
autoscaler destroyed every replica; the next request cold-started a new one in 3 ms.

### Measured (QEMU 6.2 in WSL, TCG, one CPU, 1000 Linux clones, `web-test 1000 10 2000 2000`)

Everything below ran inside a single emulated CPU (TCG, about 30x slower than native for
guest code), so the absolute numbers say more about the emulator than the design; the
ratios are the point. The guest is Linux 5.10 (128 MiB) running the Go server; requests go
through the front door proxy (Host-header routed, the same code path as SNI minus TLS);
10 concurrent clients, half of the requests to a hot set of 10 VMs, half spread over all
1000; a VM is frozen after 2 s without traffic.

| What | Result |
|---|---|
| Boot Linux + Go server, warm it | 5.4 s (once) |
| Snapshot it (`vm snapshot`) | 12 347 pages = 48 MiB captured in 11 ms, zero-copy |
| Create 1000 clones (`linux clone web vm 1000`) | 157 ms total, 0.1 ms each, 14 KiB of host memory each |
| Requests | 2000 in 253 s (7.9 req/s), 0 failed, 650 distinct VMs answered |
| Cold request (VM was frozen), 10-way load | p50 1.42 s, p90 2.96 s, p99 4.03 s |
| Warm request, 10-way load | p50 377 ms, p90 2.16 s, p99 3.29 s |
| Cold request, single client | p50 90 ms, p90 124 ms, max 198 ms (connect to a frozen clone: 64 ms avg) |
| Warm request, single client | p50 46 ms, p90 75 ms (connect: 4 ms avg) |
| Same, without the NPT prefault (`hv set prefault=0`) | cold p50 202 ms, warm p50 53 ms |
| Request counters | 50 VMs read back after the run, all exactly equal to the requests they served |
| Host memory for 1000 VMs | 132 MiB in use, of which 70 MiB are the two templates (boot image + snapshot) |
| Private memory per clone | 176 pages (704 KiB) on average, 643 max, after serving requests |
| Kernel text (3 995 pages, 15.6 MiB) | one copy shared by all; 8 pages copied per clone on average, 93 worst case (2%) |
| Freeze/thaw cycles | 1 346 freezes, 1 354 thaws; 992 of 1000 clones frozen at the end |
| Page store (disk) | 682 MiB for the frozen state of 1000 VMs; 180 k page reads, 345 k page writes |

Reading it: cloning replaces a 5.4 s boot with a 0.1 ms clone plus a 90 ms first request,
because the clone resumes a warm kernel and a warm Go runtime from shared pages; eagerly
mapping the template pages into the clone's nested page tables halves that again (2.2x)
by turning thousands of read faults into one host-side loop. Under ten-way load on one
emulated CPU everything queues behind everything else, so latencies grow about tenfold
while the failure rate stays zero and the counters stay exact across more than a thousand
freeze/thaw cycles. A frozen clone costs 14 KiB of RAM and about 700 KiB of disk; memory
for a thousand of them is dominated by the two shared templates, not by the VMs.

### After the exit diet (same test, both accelerators)

The same `web-test 1000 10 2000 2000` after the profiling work above, under TCG and under
nested KVM (`--kvm`), plus single-client figures from `vm coldstart` on a fresh clone once
the learned write set exists. Frozen clones now cost 19 KiB each (the sparse page map) and
about 1.2 MiB of disk after serving, because the learned set copies pages ahead of time.

| | TCG before | TCG after | nested KVM after |
|---|---|---|---|
| 2000 requests, 10 clients, 1000 clones | 253 s (7.9 req/s) | 66 s (30 req/s) | 113 s (17.8 req/s) |
| Cold request under load, p50 / p99 | 1.42 s / 4.03 s | 321 ms / 729 ms | 676 ms / 1.35 s |
| Warm request under load, p50 / p99 | 377 ms / 3.29 s | 287 ms / 881 ms | 311 ms / 1.42 s |
| Failures / wrong counters | 0 / 0 | 0 / 0 | 0 / 0 |
| Fresh clone, first request, single client | 90 ms | 34 ms (69 exits) | **37 ms** (95 exits, 0 page faults) |
| Re-thaw, single client | 202 ms | 20 ms (54 exits) | **36 ms** (82 exits, 234 pages prefetched in 6 ms) |
| Warm request, single client | 46 ms | 11 ms (51 exits) | **14 ms** (51 exits) |
| Snapshot / 1000 clones | 11 ms / 157 ms | 12 ms / 263 ms | 22 ms / 30 ms |

Under ten-way load nested KVM is slower than TCG because ten guests' exits all pay the
three-level reflection and the batched prefetches contend for one virtio-blk queue behind
a 9p-mounted disk image; with one client at a time it is the faster path, and its
per-request budget (51 exits warm, 95 cold) is the number that would carry over to a
non-nested KVM host, where each exit is two orders of magnitude cheaper.

## Repository layout

```text
kernel/     the OS (target x86_64-unknown-uefi, produces conc_os.efi)
guest/      the unikernel guest image (target x86_64-unknown-none, static ELF at 0x10000)
guest-web/  the Go web server Linux guests run (HTTPS/HTTP request counter, VM id hypercall)
xtask/      build/run/test driver (cargo xtask ...)
tools/      wsl-qemu.sh: private Linux QEMU inside WSL; build-webcounter.sh: the guest
            server; init-static-web.sh: an alternative guest userspace (static page)
```

Build the pieces by hand if you prefer:

```bash
cargo build --release -p guest --target x86_64-unknown-none
GUEST_ELF=$PWD/target/x86_64-unknown-none/release/guest cargo build --release -p conc_os --target x86_64-unknown-uefi
```

## Limitations and notes

* AMD SVM only. Intel VT-x is detected but not implemented (QEMU's TCG cannot emulate VT-x,
  so it could not have been tested here). On a machine without SVM the OS boots and every
  non-VM feature works; VM commands report that the hypervisor is unavailable.
* Single CPU: the boot processor runs everything. The scheduler is the async executor
  (FIFO round robin with time slices); there are no per-VM priorities yet.
* Linux guests get one vCPU, a serial console, an I/O APIC and a PIT/PIC/LAPIC, a
  virtio-mmio network device and an initramfs; there is no block device model for them, so
  the root filesystem is the initramfs. Kernels need `CONFIG_SERIAL_8250_CONSOLE`,
  `CONFIG_BLK_DEV_INITRD`, and `CONFIG_VIRTIO_MMIO` with `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`
  for networking. There is no ACPI: kernels find an MP table instead, and one built without
  `CONFIG_X86_MPPARSE` falls back to virtual-wire PIC mode (which works, minus anything it
  would have discovered from ACPI).
* Several kernels can be installed at once as named image sets and different VMs can boot
  different ones, but a single set holds one kernel, one initramfs and one command line;
  reinstalling a set repacks the whole image area (up to 256 MiB, at most 21 sets).
* TCP comes from smoltcp (Reno congestion control; CUBIC needs float intrinsics `core`
  lacks). The rest of the stack (Ethernet, ARP, IPv4, ICMP, UDP, DHCP) is our own.
* Clones share the parent's random-number state and TCP sequence secrets at the moment of
  the snapshot, exactly like restored microVM snapshots elsewhere; a real deployment would
  re-seed the guest after resume (virtio-rng or a hypercall).
* Snapshot templates are never freed (frames move into them; nothing reference-counts
  frames shared between a snapshot and its parent template).
* The console is the serial port (COM1); the framebuffer is mapped but unused.
* Tested under QEMU with `-cpu max -accel tcg` (QEMU 10 on Windows for everything but Linux
  guests, QEMU 6.2 in WSL for Linux guests, see above) and with `--kvm` (QEMU 6.2 in WSL2
  using the host's SVM, nested under KVM under Hyper-V). Bare-metal AMD boot has not been
  exercised. Under nested KVM the exit cost dominates everything (about 200 µs per
  `#VMEXIT`); a `KVM internal error, suberror 3` was printed once at QEMU start without
  visible consequences. Frames that were nested page-table pages are never handed to guests
  as data (they stay in a table pool), because KVM keeps write-protecting a frame it once
  shadowed as a table and every guest write to it would then fault forever.
