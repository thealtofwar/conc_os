//! Build/run helper for conc_os.
//!
//! ```text
//! cargo xtask build            # build guest image + kernel, populate esp/
//! cargo xtask run [opts]       # build, then boot in QEMU (serial on stdio)
//! cargo xtask test [opts]      # boot, run the built-in self tests, exit
//! cargo xtask install-linux [--name <set>] [--kernel <vmlinux|bzImage>]
//!                            [--busybox <path>] [--initrd <cpio>] [--init <script>]
//!                            [--cmdline "<...>"] [--remove <set>]
//!                              # write a Linux kernel + initramfs into the image area
//!                              # of disk.img as a named image set, keeping the other
//!                              # sets, so different VMs can boot different kernels
//!                              # (`linux create --image <set> <name>`)
//!
//! options:
//!   --cmd "<shell command>"    send a command to the conc_os shell once it
//!                              is up (repeatable; "exit" ends the session)
//!   --timeout <secs>           kill QEMU after this long (default: none for
//!                              run, 300 for test)
//!   --mem <size>               guest RAM for QEMU (default 2G)
//!   --gui                      show the QEMU window (default: headless)
//!   --no-net / --no-disk       omit the virtio devices
//!   --port <n>                 TCP port for the serial console (default 45454)
//!   --debug                    build the kernel without optimisations
//!   --wsl / --kvm              run QEMU inside WSL (Linux build); --kvm uses hardware
//!                              virtualization through /dev/kvm (nested) instead of TCG
//! ```

use std::env;
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Size of disk.img (sparse).  A 256 MiB image area sits 1 MiB in; the page
/// store owns everything else (see `kernel/src/disk/images.rs`).
const DISK_SIZE: u64 = 4 * 1024 * 1024 * 1024;
const IMAGE_AREA_OFFSET: u64 = 1024 * 1024;
const IMAGE_AREA_SIZE: u64 = 256 * 1024 * 1024;
const IMG_MAGIC: &[u8; 8] = b"CONCIMG2";
const IMG_KIND_KERNEL: u32 = 1;
const IMG_KIND_INITRD: u32 = 2;
const IMG_KIND_CMDLINE: u32 = 3;

const DEFAULT_CMDLINE: &str =
    "console=ttyS0 earlyprintk=serial,ttyS0,115200 reboot=k panic=1 tsc=reliable random.trust_cpu=on io_delay=none ipv6.disable=1 rdinit=/init loglevel=7";

struct Opts {
    cmds: Vec<String>,
    timeout: Option<u64>,
    mem: String,
    gui: bool,
    net: bool,
    disk: bool,
    port: u16,
    debug: bool,
    extra_qemu: Vec<String>,
    kernel: Option<PathBuf>,
    initrd: Option<PathBuf>,
    busybox: Option<PathBuf>,
    cmdline: Option<String>,
    /// Name of the image set `install-linux` writes (default "default").
    image_name: Option<String>,
    /// Custom `/init` script for the generated initramfs.
    init: Option<PathBuf>,
    /// Remove this image set instead of installing one.
    remove: Option<String>,
    /// Run QEMU inside WSL (Linux build; see tools/wsl-qemu.sh).
    wsl: bool,
    /// Use KVM (hardware virtualization, nested under WSL2) instead of TCG.
    /// Implies --wsl; QEMU runs as root inside WSL because /dev/kvm is
    /// group-restricted there.
    kvm: bool,
}

/// Translate a Windows path to its WSL mount point.
fn wsl_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.len() > 2 && s.as_bytes()[1] == b':' {
        let drive = s[..1].to_ascii_lowercase();
        format!("/mnt/{}{}", drive, &s[2..])
    } else {
        s
    }
}

/// Shell-quote for bash.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run_cmd(cmd: &mut Command, what: &str) {
    println!("[xtask] {}", what);
    let status = cmd.status().unwrap_or_else(|e| panic!("failed to spawn {}: {}", what, e));
    if !status.success() {
        eprintln!("[xtask] {} failed: {}", what, status);
        std::process::exit(1);
    }
}

fn build_guest(root: &Path) -> PathBuf {
    let mut c = Command::new("cargo");
    c.current_dir(root)
        .args(["build", "--release", "-p", "guest", "--target", "x86_64-unknown-none"]);
    run_cmd(&mut c, "building guest image");
    root.join("target/x86_64-unknown-none/release/guest")
}

fn build_kernel(root: &Path, guest_elf: &Path, debug: bool) -> PathBuf {
    let mut c = Command::new("cargo");
    c.current_dir(root)
        .env("GUEST_ELF", guest_elf)
        .args(["build", "-p", "conc_os", "--target", "x86_64-unknown-uefi"]);
    if !debug {
        c.arg("--release");
    }
    run_cmd(&mut c, "building kernel");
    let profile = if debug { "debug" } else { "release" };
    root.join(format!("target/x86_64-unknown-uefi/{}/conc_os.efi", profile))
}

fn make_esp(root: &Path, efi: &Path) -> PathBuf {
    let esp = root.join("esp");
    let boot = esp.join("EFI").join("BOOT");
    fs::create_dir_all(&boot).unwrap();
    fs::copy(efi, boot.join("BOOTX64.EFI")).unwrap();
    esp
}

/// Create disk.img (or grow an older, smaller one) to `DISK_SIZE`.
fn make_disk(root: &Path) -> PathBuf {
    let img = root.join("disk.img");
    let f = fs::OpenOptions::new().read(true).write(true).create(true).open(&img).unwrap();
    let len = f.metadata().unwrap().len();
    if len < DISK_SIZE {
        println!("[xtask] sizing disk image to {} MiB", DISK_SIZE >> 20);
        // Only written blocks should cost disk space: mark it sparse first
        // (Windows allocates the whole extent otherwise).
        #[cfg(windows)]
        {
            let _ = Command::new("fsutil").args(["sparse", "setflag"]).arg(&img).status();
        }
        f.set_len(DISK_SIZE).unwrap();
    }
    img
}

// ------------------------------------------------------------- initramfs ---

/// Minimal cpio "newc" archive writer.
struct Cpio {
    buf: Vec<u8>,
    ino: u32,
}

impl Cpio {
    fn new() -> Self {
        Cpio { buf: Vec::new(), ino: 1 }
    }

    fn pad4(&mut self) {
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }

    fn entry(&mut self, name: &str, mode: u32, data: &[u8], rdev: (u32, u32), nlink: u32) {
        let fields = [
            self.ino,
            mode,
            0,
            0,
            nlink,
            0,
            data.len() as u32,
            0,
            0,
            rdev.0,
            rdev.1,
            (name.len() + 1) as u32,
            0,
        ];
        self.buf.extend_from_slice(b"070701");
        for f in fields {
            self.buf.extend_from_slice(format!("{:08X}", f).as_bytes());
        }
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        self.pad4();
        self.buf.extend_from_slice(data);
        self.pad4();
        self.ino += 1;
    }

    fn dir(&mut self, name: &str) {
        self.entry(name, 0o040755, &[], (0, 0), 2);
    }
    fn file(&mut self, name: &str, mode: u32, data: &[u8]) {
        self.entry(name, 0o100000 | mode, data, (0, 0), 1);
    }
    fn symlink(&mut self, name: &str, target: &str) {
        self.entry(name, 0o120777, target.as_bytes(), (0, 0), 1);
    }
    fn chardev(&mut self, name: &str, major: u32, minor: u32) {
        self.entry(name, 0o020600, &[], (major, minor), 1);
    }
    fn finish(mut self) -> Vec<u8> {
        self.entry("TRAILER!!!", 0, &[], (0, 0), 1);
        // Archives are commonly padded to a 512-byte multiple.
        while self.buf.len() % 512 != 0 {
            self.buf.push(0);
        }
        self.buf
    }
}

// PID 1 must stay alive: a shell that exits would panic the kernel, so the
// script respawns the shell instead of exec'ing it.
const INIT_SCRIPT: &str = "#!/bin/sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null
hostname conc-linux
# The virtio-mmio NIC: every guest is 10.42.0.2 on its own link to the host.
ip link set lo up 2>/dev/null
if ip addr add 10.42.0.2/24 dev eth0 2>/dev/null; then
  ip link set eth0 up
  ip route add default via 10.42.0.1
else
  ifconfig eth0 10.42.0.2 netmask 255.255.255.0 up
  route add default gw 10.42.0.1
fi
if [ -x /bin/webcounter ]; then
  /bin/webcounter &
else
  mkdir -p /www
  echo \"hello from $(hostname) at $(cat /proc/uptime | cut -d' ' -f1)s uptime\" > /www/index.html
  httpd -p 80 -h /www
fi
echo
echo \"conc_os linux guest: $(uname -sr) booted; $(grep MemTotal /proc/meminfo)\"
echo \"busybox shell on ttyS0; 'poweroff -f' stops the VM, 'reboot -f' resets it\"
while true; do
  setsid cttyhack sh
  echo \"(shell exited; respawning)\"
done
";

/// Build a busybox initramfs.  `init` is the `/init` script; `None` uses the
/// bundled one.
fn build_initramfs(busybox: &Path, init: Option<&str>) -> Vec<u8> {
    let bb = fs::read(busybox).unwrap_or_else(|e| panic!("cannot read busybox {}: {}", busybox.display(), e));
    let mut c = Cpio::new();
    for d in ["bin", "sbin", "dev", "proc", "sys", "tmp", "etc", "root", "usr", "usr/bin"] {
        c.dir(d);
    }
    c.file("bin/busybox", 0o755, &bb);
    let web = busybox.with_file_name("webcounter");
    if let Ok(w) = fs::read(&web) {
        println!("[xtask] including guest web server {} ({} KiB)", web.display(), w.len() / 1024);
        c.file("bin/webcounter", 0o755, &w);
    } else {
        println!("[xtask] no {} (run tools/build-webcounter.sh under WSL); guests will use busybox httpd", web.display());
    }
    c.symlink("bin/sh", "busybox");
    c.chardev("dev/console", 5, 1);
    c.chardev("dev/null", 1, 3);
    c.chardev("dev/ttyS0", 4, 64);
    c.file("init", 0o755, init.unwrap_or(INIT_SCRIPT).as_bytes());
    c.file("etc/hostname", 0o644, b"conc-linux\n");
    c.finish()
}

/// One entry of the on-disk image directory.
struct DirEntry {
    name: String,
    kind: u32,
    offset: u64,
    size: u64,
}

/// Read the image directory of `disk.img` (empty if unformatted).
fn read_directory(f: &mut fs::File) -> Vec<DirEntry> {
    let mut dir = vec![0u8; 4096];
    if f.seek(SeekFrom::Start(IMAGE_AREA_OFFSET)).is_err() || f.read_exact(&mut dir).is_err() || &dir[..8] != IMG_MAGIC {
        return Vec::new();
    }
    let count = u32::from_le_bytes([dir[8], dir[9], dir[10], dir[11]]) as usize;
    let mut out = Vec::new();
    for i in 0..count.min(63) {
        let e = &dir[16 + i * 64..16 + (i + 1) * 64];
        let name_end = e[..32].iter().position(|&b| b == 0).unwrap_or(32);
        let name = String::from_utf8_lossy(&e[..name_end]).to_string();
        let kind = u32::from_le_bytes([e[32], e[33], e[34], e[35]]);
        let offset = u64::from_le_bytes(e[40..48].try_into().unwrap());
        let size = u64::from_le_bytes(e[48..56].try_into().unwrap());
        if size > 0 && offset >= IMAGE_AREA_OFFSET + 4096 {
            out.push(DirEntry { name, kind, offset, size });
        }
    }
    out
}

/// Rewrite the image area with exactly `images` (name, kind, bytes), packed
/// from the start of the data area.
fn write_directory(f: &mut fs::File, images: &[(String, u32, Vec<u8>)]) {
    if images.len() > 63 {
        panic!("the image directory holds at most 63 entries ({} given)", images.len());
    }
    let mut dir = vec![0u8; 4096];
    dir[..8].copy_from_slice(IMG_MAGIC);
    dir[8..12].copy_from_slice(&(images.len() as u32).to_le_bytes());
    let mut offset = IMAGE_AREA_OFFSET + 4096;
    for (i, (name, kind, data)) in images.iter().enumerate() {
        if name.len() > 32 {
            panic!("image name '{}' is longer than 32 bytes", name);
        }
        let e = &mut dir[16 + i * 64..16 + (i + 1) * 64];
        e[..name.len()].copy_from_slice(name.as_bytes());
        e[32..36].copy_from_slice(&kind.to_le_bytes());
        e[40..48].copy_from_slice(&offset.to_le_bytes());
        e[48..56].copy_from_slice(&(data.len() as u64).to_le_bytes());
        if offset + data.len() as u64 > IMAGE_AREA_OFFSET + IMAGE_AREA_SIZE {
            panic!(
                "images do not fit in the {} MiB image area; remove one with --remove <set>",
                IMAGE_AREA_SIZE >> 20
            );
        }
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(data).unwrap();
        offset = (offset + data.len() as u64 + 4095) & !4095;
    }
    f.seek(SeekFrom::Start(IMAGE_AREA_OFFSET)).unwrap();
    f.write_all(&dir).unwrap();
    f.flush().unwrap();
}

/// Print the installed sets the way the shell's `linux images` does.
fn print_sets(images: &[(String, u32, Vec<u8>)]) {
    let mut names: Vec<&String> = Vec::new();
    for (n, k, _) in images {
        if *k == IMG_KIND_KERNEL {
            names.push(n);
        }
    }
    println!("[xtask] installed images ({} sets, first is the default):", names.len());
    for n in names {
        let size = |k: u32| images.iter().find(|(m, j, _)| m == n && *j == k).map(|(_, _, d)| d.len()).unwrap_or(0);
        let cmdline = images
            .iter()
            .find(|(m, j, _)| m == n && *j == IMG_KIND_CMDLINE)
            .map(|(_, _, d)| String::from_utf8_lossy(d).to_string())
            .unwrap_or_default();
        println!(
            "  {:<14} kernel {:>6} KiB   initramfs {:>6} KiB   \"{}\"",
            n,
            size(IMG_KIND_KERNEL) / 1024,
            size(IMG_KIND_INITRD) / 1024,
            cmdline
        );
    }
}

/// Write a named kernel + initramfs + command line into the image area of
/// disk.img, keeping the other installed sets.  `--remove <set>` deletes one
/// instead.
fn install_linux(root: &Path, opts: &Opts) {
    let img = make_disk(root);
    let mut f = fs::OpenOptions::new().read(true).write(true).open(&img).unwrap();

    // Everything already installed, minus the set we are about to write.
    let set_name = opts.image_name.clone().unwrap_or_else(|| String::from("default"));
    let doomed = opts.remove.clone().unwrap_or_else(|| set_name.clone());
    let existing = read_directory(&mut f);
    let mut images: Vec<(String, u32, Vec<u8>)> = Vec::new();
    for e in existing.iter().filter(|e| e.name != doomed) {
        let mut buf = vec![0u8; e.size as usize];
        f.seek(SeekFrom::Start(e.offset)).unwrap();
        f.read_exact(&mut buf).unwrap();
        images.push((e.name.clone(), e.kind, buf));
    }
    if let Some(name) = &opts.remove {
        if existing.iter().all(|e| &e.name != name) {
            println!("[xtask] no image set '{}' installed", name);
        }
        write_directory(&mut f, &images);
        println!("[xtask] removed image set '{}' from {}", name, img.display());
        print_sets(&images);
        return;
    }

    let kernel = opts
        .kernel
        .clone()
        .or_else(|| {
            let dir = root.join("images");
            fs::read_dir(&dir).ok().and_then(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        let n = p.file_name().unwrap().to_string_lossy().to_string();
                        (n.starts_with("vmlinux") || n.starts_with("bzImage")) && !n.ends_with(".config")
                    })
                    .next()
            })
        })
        .expect("no kernel: pass --kernel <vmlinux|bzImage> or put one in images/");
    let kernel_bytes = fs::read(&kernel).unwrap_or_else(|e| panic!("cannot read kernel {}: {}", kernel.display(), e));

    let init_script = opts
        .init
        .as_ref()
        .map(|p| fs::read_to_string(p).unwrap_or_else(|e| panic!("cannot read init script {}: {}", p.display(), e)));
    let initrd_bytes = match &opts.initrd {
        Some(p) => fs::read(p).unwrap_or_else(|e| panic!("cannot read initrd {}: {}", p.display(), e)),
        None => {
            let bb = opts.busybox.clone().unwrap_or_else(|| root.join("images").join("busybox"));
            println!("[xtask] building busybox initramfs from {}", bb.display());
            build_initramfs(&bb, init_script.as_deref())
        }
    };
    let cmdline = opts.cmdline.clone().unwrap_or_else(|| DEFAULT_CMDLINE.to_string());

    images.push((set_name.clone(), IMG_KIND_KERNEL, kernel_bytes.clone()));
    images.push((set_name.clone(), IMG_KIND_INITRD, initrd_bytes.clone()));
    images.push((set_name.clone(), IMG_KIND_CMDLINE, cmdline.clone().into_bytes()));
    write_directory(&mut f, &images);
    println!(
        "[xtask] installed image '{}': kernel {} ({} KiB), initramfs ({} KiB), cmdline \"{}\" into {}",
        set_name,
        kernel.display(),
        kernel_bytes.len() / 1024,
        initrd_bytes.len() / 1024,
        cmdline,
        img.display()
    );
    print_sets(&images);
}

// ------------------------------------------------------------------ qemu ---

fn find_qemu() -> (PathBuf, PathBuf) {
    let exe_name = if cfg!(windows) { "qemu-system-x86_64.exe" } else { "qemu-system-x86_64" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(q) = env::var("QEMU") {
        candidates.push(PathBuf::from(q));
    }
    if let Ok(path) = env::var("PATH") {
        for dir in env::split_paths(&path) {
            candidates.push(dir.join(exe_name));
        }
    }
    candidates.push(PathBuf::from(r"C:\Program Files\qemu").join(exe_name));
    candidates.push(PathBuf::from("/usr/bin").join(exe_name));
    let qemu = candidates.into_iter().find(|p| p.exists()).expect("qemu-system-x86_64 not found; set QEMU=<path>");

    let mut fw: Vec<PathBuf> = Vec::new();
    if let Ok(f) = env::var("OVMF_CODE") {
        fw.push(PathBuf::from(f));
    }
    let qdir = qemu.parent().unwrap();
    fw.push(qdir.join("share").join("edk2-x86_64-code.fd"));
    fw.push(qdir.join("..").join("share").join("qemu").join("edk2-x86_64-code.fd"));
    fw.push(PathBuf::from("/usr/share/qemu/edk2-x86_64-code.fd"));
    fw.push(PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"));
    fw.push(PathBuf::from("/usr/share/edk2/ovmf/OVMF_CODE.fd"));
    let firmware = fw.into_iter().find(|p| p.exists()).expect("UEFI firmware (edk2-x86_64-code.fd) not found; set OVMF_CODE=<path>");
    (qemu, firmware)
}

/// The QEMU argument list shared by the native and WSL launch paths.
fn qemu_args(opts: &Opts, fw: &str, esp: &str, disk: &str) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &str| a.push(s.to_string());
    push("-machine");
    push("q35");
    push("-accel");
    push(if opts.kvm { "kvm" } else { "tcg,thread=single" });
    push("-cpu");
    push(if opts.kvm { "host" } else { "max" });
    push("-smp");
    push("1");
    push("-m");
    push(&opts.mem);
    push("-drive");
    push(&format!("if=pflash,format=raw,readonly=on,file={}", fw));
    push("-drive");
    push(&format!("format=raw,file=fat:rw:{}", esp));
    push("-serial");
    push(&format!("tcp:127.0.0.1:{},server=on,wait=off", opts.port));
    push("-device");
    push("isa-debug-exit,iobase=0xf4,iosize=0x04");
    push("-no-reboot");
    if !opts.gui {
        push("-display");
        push("none");
    }
    if opts.net {
        push("-netdev");
        push("user,id=n0,hostfwd=udp::7777-:7777,hostfwd=tcp::8443-:443,hostfwd=tcp::8080-:80");
        push("-device");
        push("virtio-net-pci,netdev=n0,disable-modern=on,mac=52:54:00:12:34:56");
    }
    if opts.disk {
        push("-drive");
        push(&format!("file={},format=raw,if=none,id=d0", disk));
        push("-device");
        push("virtio-blk-pci,drive=d0,disable-modern=on");
    }
    for x in &opts.extra_qemu {
        push(x);
    }
    a
}

fn qemu_command(opts: &Opts, esp: &Path, disk: &Path) -> Command {
    if opts.wsl {
        return wsl_qemu_command(opts, esp, disk);
    }
    let (qemu, fw) = find_qemu();
    let mut c = Command::new(qemu);
    c.args(qemu_args(opts, &fw.to_string_lossy(), &esp.to_string_lossy(), &disk.to_string_lossy()));
    c
}

/// Launch the private Linux QEMU inside WSL (set up by tools/wsl-qemu.sh).
/// The Windows QEMU build truncates SVM segment bases (32-bit `long`), which
/// breaks Linux guests; the Linux build does not.
fn wsl_qemu_command(opts: &Opts, esp: &Path, disk: &Path) -> Command {
    let home = Command::new("wsl.exe")
        .args(["-e", "bash", "-c", "echo -n $HOME"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if home.is_empty() {
        eprintln!("[xtask] cannot determine the WSL home directory");
        std::process::exit(1);
    }
    let prefix = format!("{}/.conc_os/qemu/root", home);
    let qemu = format!("{}/usr/bin/qemu-system-x86_64", prefix);
    let fw = format!("{}/usr/share/OVMF/OVMF_CODE_4M.fd", prefix);
    let mut args: Vec<String> = vec![
        sh_quote(&qemu),
        "-L".into(),
        sh_quote(&format!("{}/usr/share/seabios", prefix)),
        "-L".into(),
        sh_quote(&format!("{}/usr/lib/ipxe/qemu", prefix)),
        "-L".into(),
        sh_quote("/usr/share/qemu"),
    ];
    for a in qemu_args(opts, &fw, &wsl_path(esp), &wsl_path(disk)) {
        args.push(sh_quote(&a));
    }
    let script = format!(
        "if [ ! -x {q} ]; then echo 'WSL qemu not set up: run tools/wsl-qemu.sh inside WSL' >&2; exit 2; fi; \
         export LD_LIBRARY_PATH={lib1}:{lib2}; export QEMU_MODULE_DIR={moddir}; exec {args}",
        q = sh_quote(&qemu),
        lib1 = sh_quote(&format!("{}/usr/lib/x86_64-linux-gnu", prefix)),
        lib2 = sh_quote(&format!("{}/lib/x86_64-linux-gnu", prefix)),
        moddir = sh_quote(&format!("{}/usr/lib/x86_64-linux-gnu/qemu", prefix)),
        args = args.join(" ")
    );
    let mut c = Command::new("wsl.exe");
    if opts.kvm {
        c.args(["-u", "root"]);
    }
    c.args(["-e", "bash", "-c", &script]);
    c
}

/// Bridge the QEMU serial console to our stdio.  Returns the exit code QEMU
/// reported (via isa-debug-exit, or the process status).
fn run_qemu(mut cmd: Command, opts: &Opts) -> i32 {
    println!("[xtask] launching QEMU ({} mode)", if opts.gui { "gui" } else { "headless" });
    let mut child = cmd.stdin(Stdio::null()).spawn().expect("failed to start qemu");

    // Connect to the serial console.
    let deadline = Instant::now() + Duration::from_secs(15);
    let stream = loop {
        match TcpStream::connect(("127.0.0.1", opts.port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                let _ = child.kill();
                panic!("could not connect to QEMU serial port: {}", e);
            }
        }
    };
    stream.set_nodelay(true).ok();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = stream;

    // Reader thread: echo console output, detect prompts.
    let (prompt_tx, prompt_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut line: Vec<u8> = Vec::new();
        let stdout = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut out = stdout.lock();
                    out.write_all(&buf[..n]).ok();
                    out.flush().ok();
                    for &b in &buf[..n] {
                        if b == b'\n' {
                            line.clear();
                        } else {
                            line.push(b);
                            if line.ends_with(b"conc_os> ") {
                                let _ = prompt_tx.send(());
                                line.clear();
                            }
                        }
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });

    // Feed commands (scripted or interactive).
    let scripted = !opts.cmds.is_empty();
    let cmds = opts.cmds.clone();
    thread::spawn(move || {
        if scripted {
            for c in cmds {
                // Lines starting with '@' are sent to the guest console after a
                // pause instead of waiting for the conc_os prompt (used while a
                // VM console is attached).
                if let Some(raw) = c.strip_prefix('@') {
                    let (delay, text) = match raw.split_once(':') {
                        Some((d, t)) if d.chars().all(|ch| ch.is_ascii_digit()) => (d.parse::<u64>().unwrap_or(0), t.to_string()),
                        _ => (2000, raw.to_string()),
                    };
                    thread::sleep(Duration::from_millis(delay));
                    let _ = writer.write_all(text.as_bytes());
                    let _ = writer.write_all(b"\n");
                    let _ = writer.flush();
                    continue;
                }
                if prompt_rx.recv().is_err() {
                    return;
                }
                let _ = writer.write_all(c.as_bytes());
                let _ = writer.write_all(b"\n");
                let _ = writer.flush();
            }
        } else {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let _ = writer.write_all(line.as_bytes());
                let _ = writer.write_all(b"\n");
                let _ = writer.flush();
            }
        }
    });

    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait failed") {
            break Some(st);
        }
        if let Some(t) = opts.timeout {
            if start.elapsed() > Duration::from_secs(t) {
                eprintln!("\n[xtask] timeout after {} s; killing QEMU", t);
                let _ = child.kill();
                let _ = child.wait();
                if opts.wsl {
                    let mut k = Command::new("wsl.exe");
                    if opts.kvm {
                        k.args(["-u", "root"]);
                    }
                    let _ = k.args(["-e", "pkill", "-f", "qemu-system-x86_64"]).status();
                }
                break None;
            }
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = done_rx.recv_timeout(Duration::from_millis(500));
    println!();
    match status {
        None => 124,
        Some(st) => {
            let code = st.code().unwrap_or(-1);
            // isa-debug-exit: status = (value << 1) | 1
            if code & 1 == 1 && code > 1 {
                let v = code >> 1;
                println!("[xtask] conc_os exited with code {}", v);
                match v {
                    1 => 0,
                    3 => 2, // panic
                    _ => 1,
                }
            } else {
                println!("[xtask] QEMU exited with status {}", code);
                code
            }
        }
    }
}

fn parse_opts(args: &[String]) -> Opts {
    let mut o = Opts {
        cmds: Vec::new(),
        timeout: None,
        mem: "2G".into(),
        gui: false,
        net: true,
        disk: true,
        port: 45454,
        debug: false,
        extra_qemu: Vec::new(),
        kernel: None,
        initrd: None,
        busybox: None,
        cmdline: None,
        image_name: None,
        init: None,
        remove: None,
        wsl: false,
        kvm: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--wsl" => o.wsl = true,
            "--kvm" => {
                o.kvm = true;
                o.wsl = true;
            }
            "--cmd" => {
                i += 1;
                o.cmds.push(args[i].clone());
            }
            "--timeout" => {
                i += 1;
                o.timeout = Some(args[i].parse().expect("timeout"));
            }
            "--mem" => {
                i += 1;
                o.mem = args[i].clone();
            }
            "--port" => {
                i += 1;
                o.port = args[i].parse().expect("port");
            }
            "--kernel" => {
                i += 1;
                o.kernel = Some(PathBuf::from(&args[i]));
            }
            "--initrd" => {
                i += 1;
                o.initrd = Some(PathBuf::from(&args[i]));
            }
            "--busybox" => {
                i += 1;
                o.busybox = Some(PathBuf::from(&args[i]));
            }
            "--cmdline" => {
                i += 1;
                o.cmdline = Some(args[i].clone());
            }
            "--name" | "--image" => {
                i += 1;
                o.image_name = Some(args[i].clone());
            }
            "--init" => {
                i += 1;
                o.init = Some(PathBuf::from(&args[i]));
            }
            "--remove" => {
                i += 1;
                o.remove = Some(args[i].clone());
            }
            "--gui" => o.gui = true,
            "--no-net" => o.net = false,
            "--no-disk" => o.disk = false,
            "--debug" => o.debug = true,
            "--qemu-arg" => {
                i += 1;
                o.extra_qemu.push(args[i].clone());
            }
            other => {
                eprintln!("unknown option {}", other);
                std::process::exit(2);
            }
        }
        i += 1;
    }
    o
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("build");
    let mut opts = parse_opts(&args[1.min(args.len())..]);
    let root = root();

    match sub {
        "build" => {
            let guest = build_guest(&root);
            let efi = build_kernel(&root, &guest, opts.debug);
            let esp = make_esp(&root, &efi);
            make_disk(&root);
            println!("[xtask] kernel: {}", efi.display());
            println!("[xtask] esp:    {}", esp.display());
        }
        "run" | "test" => {
            if sub == "test" {
                if opts.cmds.is_empty() {
                    if cfg!(windows) && !opts.wsl {
                        // The Windows QEMU build truncates SVM segment bases
                        // (32-bit `long`), so Linux guests cannot run on it.
                        println!("[xtask] note: Linux guest tests are skipped on the Windows QEMU build; use --wsl to run them");
                        opts.cmds.push("hv set linux_tests=0".into());
                    }
                    opts.cmds.push("selftest".into());
                    opts.cmds.push("exit".into());
                }
                if opts.timeout.is_none() {
                    opts.timeout = Some(if opts.wsl { 1500 } else { 300 });
                }
            }
            let guest = build_guest(&root);
            let efi = build_kernel(&root, &guest, opts.debug);
            let esp = make_esp(&root, &efi);
            let disk = make_disk(&root);
            let cmd = qemu_command(&opts, &esp, &disk);
            let code = run_qemu(cmd, &opts);
            std::process::exit(code);
        }
        "install-linux" => {
            install_linux(&root, &opts);
        }
        "clean-disk" => {
            let _ = fs::remove_file(root.join("disk.img"));
        }
        other => {
            eprintln!("unknown subcommand {}", other);
            std::process::exit(2);
        }
    }
}
