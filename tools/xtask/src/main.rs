//! Build/run/test automation for HARLAN OS. See ROADMAP.md Fase 0: the goal is
//! a single command that builds and boots the image in QEMU.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};

const UEFI_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
/// Must match `kernel::shell::SHELL_READY_MARKER` (kernel/src/shell.rs).
/// Not shared via a dependency edge because xtask is host tooling, not part
/// of the freestanding boot chain. Override with `--marker` to check the
/// older Fase 0 checkpoint (`HARLAN-PHASE0-BOOT-OK`) instead.
const DEFAULT_MARKER: &str = "HARLAN-PHASE1-SHELL-READY";
/// Guest RAM the frame allocator's fixed bitmap is sized for (see
/// kernel::memory::FRAME_BITMAP_WORDS). More RAM boots too; the frames
/// above the covered range are ignored and logged.
const DEFAULT_MEMORY: &str = "256M";

#[derive(Parser)]
#[command(name = "xtask", about = "HARLAN OS build/run/test automation")]
struct Cli {
    #[command(subcommand)]
    command: XtaskCommand,
}

#[derive(Subcommand)]
enum XtaskCommand {
    /// Build the kernel and the UEFI boot application, and assemble the ESP directory.
    Build,
    /// Build and run HARLAN OS interactively in QEMU (opens a window).
    Run,
    /// Run host-runnable unit tests (every crate except `harlan-boot`, which
    /// only builds for UEFI).
    Test,
    /// Run cargo fmt and cargo clippy across every target.
    FmtLint {
        /// Apply formatting fixes instead of only checking.
        #[arg(long)]
        fix: bool,
    },
    /// Run QEMU with a paused CPU and a GDB stub for source-level debugging.
    Debug,
    /// Run QEMU headlessly and fail unless the boot marker appears before the timeout.
    BootTest {
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        #[arg(long, default_value = DEFAULT_MARKER)]
        marker: String,
        /// Repeat the boot this many times, failing on the first attempt
        /// that doesn't reach the marker in time.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Build with the `heap-stress` feature: the boot self-test runs the
        /// long heap stress workload instead of the quick one.
        #[arg(long)]
        heap_stress: bool,
        /// Guest RAM, in QEMU's own syntax.
        #[arg(long, default_value = DEFAULT_MEMORY)]
        memory: String,
    },
    /// Boot a soak build (endless heap stress rounds instead of the shell)
    /// and let it run for the whole duration. Fails on an early exit, a CPU
    /// reset after boot, a panic or exception, a boot marker seen twice, or
    /// too little progress (timer ticks, heap cycles).
    SoakTest {
        #[arg(long, default_value_t = 120)]
        duration_secs: u64,
        #[arg(long, default_value_t = 10_000)]
        min_ticks: u64,
        #[arg(long, default_value_t = 50_000)]
        min_heap_cycles: u64,
        /// Guest RAM, in QEMU's own syntax.
        #[arg(long, default_value = DEFAULT_MEMORY)]
        memory: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = workspace_root();

    match cli.command {
        XtaskCommand::Build => build(&root, &[]),
        XtaskCommand::Run => run(&root),
        XtaskCommand::Test => test(&root),
        XtaskCommand::FmtLint { fix } => fmt_lint(&root, fix),
        XtaskCommand::Debug => debug(&root),
        XtaskCommand::BootTest {
            timeout_secs,
            marker,
            repeat,
            heap_stress,
            memory,
        } => boot_test(
            &root,
            Duration::from_secs(timeout_secs),
            &marker,
            repeat,
            heap_stress,
            &memory,
        ),
        XtaskCommand::SoakTest {
            duration_secs,
            min_ticks,
            min_heap_cycles,
            memory,
        } => soak_test(
            &root,
            Duration::from_secs(duration_secs),
            min_ticks,
            min_heap_cycles,
            &memory,
        ),
    }
}

fn workspace_root() -> PathBuf {
    // tools/xtask -> tools -> <workspace root>
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tools/xtask is two directories below the workspace root")
        .to_path_buf()
}

fn run_cargo(root: &Path, args: &[&str]) -> Result<()> {
    let manifest_path = root.join("Cargo.toml");
    let (subcommand, rest) = args
        .split_first()
        .context("run_cargo called with no subcommand")?;
    let status = Command::new("cargo")
        .arg(subcommand)
        .arg("--manifest-path")
        .arg(&manifest_path)
        .args(rest)
        .status()
        .context("failed to spawn cargo")?;
    if !status.success() {
        bail!("cargo {args:?} failed with {status}");
    }
    Ok(())
}

fn build(root: &Path, features: &[&str]) -> Result<()> {
    for command in build_commands(features) {
        let args: Vec<&str> = command.iter().map(String::as_str).collect();
        run_cargo(root, &args)?;
    }
    assemble_esp(root)
}

/// The cargo invocations `build` runs, with `features` enabled on both
/// crates (`harlan-boot` forwards each one to `harlan-kernel`). Pure so the
/// feature plumbing is unit-testable.
fn build_commands(features: &[&str]) -> [Vec<String>; 2] {
    [
        ("harlan-kernel", KERNEL_TARGET),
        ("harlan-boot", UEFI_TARGET),
    ]
    .map(|(package, target)| {
        let mut command: Vec<String> = ["build", "-p", package, "--target", target]
            .map(String::from)
            .into();
        if !features.is_empty() {
            command.push("--features".to_string());
            command.push(features.join(","));
        }
        command
    })
}

fn assemble_esp(root: &Path) -> Result<()> {
    let efi_src = root
        .join("target")
        .join(UEFI_TARGET)
        .join("debug")
        .join("harlan-boot.efi");
    let esp_boot_dir = root.join("target").join("esp").join("efi").join("boot");
    fs::create_dir_all(&esp_boot_dir)
        .with_context(|| format!("failed to create {}", esp_boot_dir.display()))?;
    let esp_dest = esp_boot_dir.join("bootx64.efi");
    fs::copy(&efi_src, &esp_dest).with_context(|| {
        format!(
            "failed to copy {} into {}",
            efi_src.display(),
            esp_dest.display()
        )
    })?;
    Ok(())
}

fn test(root: &Path) -> Result<()> {
    run_cargo(
        root,
        &[
            "test",
            "-p",
            "harlan-hal",
            "-p",
            "harlan-arch-x86_64",
            "-p",
            "harlan-fbcon",
            "-p",
            "harlan-kernel",
            "-p",
            "xtask",
        ],
    )
}

fn fmt_lint(root: &Path, fix: bool) -> Result<()> {
    if fix {
        run_cargo(root, &["fmt", "--all"])?;
    } else {
        run_cargo(root, &["fmt", "--all", "--", "--check"])?;
    }
    run_cargo(
        root,
        &[
            "clippy",
            "-p",
            "harlan-hal",
            "-p",
            "harlan-arch-x86_64",
            "-p",
            "harlan-fbcon",
            "-p",
            "harlan-kernel",
            "-p",
            "xtask",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run_cargo(
        root,
        &[
            "clippy",
            "-p",
            "harlan-kernel",
            "--target",
            KERNEL_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run_cargo(
        root,
        &[
            "clippy",
            "-p",
            "harlan-boot",
            "--target",
            UEFI_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// Downloads (and caches) prebuilt OVMF firmware. Requires network on first
/// run only; see SETUP.md for the offline fallback of placing `code.fd` /
/// `vars.fd` under `target/ovmf/x64/` by hand.
fn fetch_ovmf(root: &Path) -> Result<(PathBuf, PathBuf)> {
    let ovmf_dir = root.join("target").join("ovmf");
    let ovmf_dir_str = ovmf_dir
        .to_str()
        .context("workspace path is not valid UTF-8")?;
    let prebuilt = Prebuilt::fetch(Source::LATEST, ovmf_dir_str)
        .context("failed to fetch OVMF prebuilt firmware")?;
    let code = prebuilt.get_file(Arch::X64, FileType::Code).to_path_buf();
    let vars_template = prebuilt.get_file(Arch::X64, FileType::Vars).to_path_buf();
    Ok((code, vars_template))
}

/// OVMF vars must be writable NVRAM storage, never the shared read-only
/// prebuilt copy, so each run gets its own scratch copy.
fn prepare_vars_copy(root: &Path, vars_template: &Path) -> Result<PathBuf> {
    let scratch = root.join("target").join("ovmf-vars.fd");
    fs::copy(vars_template, &scratch).with_context(|| {
        format!(
            "failed to copy {} to {}",
            vars_template.display(),
            scratch.display()
        )
    })?;
    Ok(scratch)
}

struct QemuConfig {
    ovmf_code: PathBuf,
    ovmf_vars: PathBuf,
    esp_dir: PathBuf,
    /// A disk for the kernel to find on the PCI bus and, from Fase 4 on,
    /// to read (docs/adr/0022-fase4-pci-enumeration.md).
    disk: PathBuf,
    memory: String,
    headless: bool,
    debug_stub: bool,
    debugcon_log: Option<PathBuf>,
    /// QEMU's own diagnostics (`-d guest_errors,cpu_reset`), for soak runs.
    qemu_log: Option<PathBuf>,
}

/// Pure function so the argument construction is unit-testable without
/// actually spawning QEMU.
fn build_qemu_args(cfg: &QemuConfig) -> Vec<String> {
    let mut args = vec![
        "-machine".to_string(),
        "pc".to_string(),
        "-cpu".to_string(),
        "qemu64".to_string(),
        "-m".to_string(),
        cfg.memory.clone(),
        "-accel".to_string(),
        "tcg".to_string(),
        // No network device at all: enforces "no red para arrancar" (ARCHITECTURE.md)
        // in a way that's verifiable, not just documented.
        "-net".to_string(),
        "none".to_string(),
        "-drive".to_string(),
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            cfg.ovmf_code.display()
        ),
        "-drive".to_string(),
        format!("if=pflash,format=raw,file={}", cfg.ovmf_vars.display()),
        "-drive".to_string(),
        format!("format=raw,file=fat:rw:{}", cfg.esp_dir.display()),
        // The disk, as a virtio device rather than an emulated IDE
        // controller: it is the one the kernel is going to learn to talk
        // to, and it is the one the ROADMAP calls "almacenamiento
        // virtual". Nothing boots from it; the firmware boots from the
        // ESP above.
        "-drive".to_string(),
        format!(
            "format=raw,file={},if=none,id=harlan-disk",
            cfg.disk.display()
        ),
        "-device".to_string(),
        // Modern-only. While the legacy interface is there the device is
        // "transitional" and a driver with a mistake can work by the old
        // path, which would make the test say nothing
        // (docs/adr/0023-fase4-device-registers.md).
        "virtio-blk-pci,drive=harlan-disk,disable-legacy=on,disable-modern=off".to_string(),
        // Must match the fixed port the `uefi` crate's `log-debugcon` feature
        // writes to (0xE9, the "debugcon"/Bochs-style debug port), not the
        // Bochs-BIOS-info-port default of 0x402.
        "-global".to_string(),
        "isa-debugcon.iobase=0xe9".to_string(),
    ];

    match &cfg.debugcon_log {
        Some(log_path) => {
            args.push("-debugcon".to_string());
            args.push(format!("file:{}", log_path.display()));
        }
        None => {
            args.push("-debugcon".to_string());
            args.push("stdio".to_string());
        }
    }

    if cfg.headless {
        args.push("-display".to_string());
        args.push("none".to_string());
    }

    if cfg.debug_stub {
        args.push("-s".to_string());
        args.push("-S".to_string());
    }

    if let Some(qemu_log) = &cfg.qemu_log {
        args.push("-d".to_string());
        args.push("guest_errors,cpu_reset".to_string());
        args.push("-D".to_string());
        args.push(qemu_log.display().to_string());
    }

    args
}

fn qemu_binary() -> &'static str {
    "qemu-system-x86_64"
}

/// How big the disk is. Small on purpose: it is written on every build and
/// read one sector at a time.
const DISK_BYTES: u64 = 8 * 1024 * 1024;
/// What the first sector holds, so that a driver reading it can say
/// whether it read the right thing rather than "something".
const DISK_SIGNATURE: &[u8] = b"HARLAN-DISK-0\n";

/// The disk QEMU attaches: raw, and mostly zeroes until Fase 4 puts a
/// filesystem on it. The signature in sector 0 is what makes a read
/// verifiable — a driver that reads it can say whether it read the *right*
/// thing rather than "something".
///
/// The file is created and sized only when it is missing or the wrong
/// size, so whatever a later increment writes to the rest of it survives.
/// The first sector is written every time, because a run that corrupted it
/// would otherwise leave every run after it quietly checking against
/// rubbish.
fn prepare_disk(root: &Path) -> Result<PathBuf> {
    let path = root.join("target").join("disk.img");
    let right_size = fs::metadata(&path).is_ok_and(|disk| disk.len() == DISK_BYTES);
    if !right_size {
        fs::create_dir_all(path.parent().expect("target has a parent"))
            .context("failed to create the target directory for the disk")?;
        fs::File::create(&path)
            .with_context(|| format!("failed to create the disk at {}", path.display()))?
            .set_len(DISK_BYTES)
            .context("failed to size the disk image")?;
    }
    let mut sector = [0u8; 512];
    sector[..DISK_SIGNATURE.len()].copy_from_slice(DISK_SIGNATURE);
    // The last two bytes of a boot sector, so that anything else looking
    // at this image recognises the shape even though nothing boots from it.
    sector[510] = 0x55;
    sector[511] = 0xAA;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .context("failed to open the disk image to write its first sector")?
        .write_all(&sector)
        .context("failed to write the disk signature")?;
    Ok(path)
}

fn prepare_qemu_config(
    root: &Path,
    headless: bool,
    debug_stub: bool,
    memory: &str,
) -> Result<QemuConfig> {
    let (ovmf_code, ovmf_vars_template) = fetch_ovmf(root)?;
    let ovmf_vars = prepare_vars_copy(root, &ovmf_vars_template)?;
    let disk = prepare_disk(root)?;
    Ok(QemuConfig {
        ovmf_code,
        ovmf_vars,
        esp_dir: root.join("target").join("esp"),
        disk,
        memory: memory.to_string(),
        headless,
        debug_stub,
        debugcon_log: None,
        qemu_log: None,
    })
}

fn run(root: &Path) -> Result<()> {
    build(root, &[])?;
    let cfg = prepare_qemu_config(root, false, false, DEFAULT_MEMORY)?;
    let args = build_qemu_args(&cfg);
    let status = Command::new(qemu_binary())
        .args(&args)
        .status()
        .with_context(|| format!("failed to launch {}", qemu_binary()))?;
    if !status.success() {
        bail!("{} exited with {status}", qemu_binary());
    }
    Ok(())
}

fn debug(root: &Path) -> Result<()> {
    build(root, &[])?;
    let cfg = prepare_qemu_config(root, false, true, DEFAULT_MEMORY)?;
    let args = build_qemu_args(&cfg);
    println!("QEMU paused at reset; gdbstub listening on tcp::1234 (see .vscode/launch.json)");
    let status = Command::new(qemu_binary())
        .args(&args)
        .status()
        .with_context(|| format!("failed to launch {}", qemu_binary()))?;
    if !status.success() {
        bail!("{} exited with {status}", qemu_binary());
    }
    Ok(())
}

fn boot_test(
    root: &Path,
    timeout: Duration,
    marker: &str,
    repeat: u32,
    heap_stress: bool,
    memory: &str,
) -> Result<()> {
    build(root, if heap_stress { &["heap-stress"] } else { &[] })?;
    for attempt in 1..=repeat {
        boot_test_once(root, timeout, marker, memory)
            .with_context(|| format!("boot-test attempt {attempt}/{repeat} failed"))?;
    }
    println!("boot-test: {repeat}/{repeat} consecutive successful boots (marker {marker:?})");
    Ok(())
}

fn boot_test_once(root: &Path, timeout: Duration, marker: &str, memory: &str) -> Result<()> {
    let mut cfg = prepare_qemu_config(root, true, false, memory)?;
    let log_path = root.join("target").join("boot-test.log");
    let stderr_path = root.join("target").join("boot-test-qemu-stderr.log");
    if log_path.exists() {
        fs::remove_file(&log_path).context("failed to clear previous boot-test log")?;
    }
    cfg.debugcon_log = Some(log_path.clone());
    let args = build_qemu_args(&cfg);

    let stderr_file = fs::File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let mut child = Command::new(qemu_binary())
        .args(&args)
        .stdout(Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .with_context(|| {
            format!(
                "failed to launch {} (is QEMU installed and on PATH?)",
                qemu_binary()
            )
        })?;

    let start = Instant::now();
    let found = loop {
        if log_path
            .metadata()
            .is_ok_and(|_| fs::read_to_string(&log_path).is_ok_and(|c| c.contains(marker)))
        {
            break true;
        }
        if let Some(status) = child.try_wait().context("failed to poll qemu status")? {
            eprintln!("qemu exited early with {status} before the timeout elapsed");
            break false;
        }
        if start.elapsed() > timeout {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let _ = child.kill();
    let _ = child.wait();

    if found {
        println!(
            "boot-test: marker {marker:?} observed after {:?}",
            start.elapsed()
        );
        Ok(())
    } else {
        bail!(
            "boot-test: marker {marker:?} NOT observed within {timeout:?} (see {} and {})",
            log_path.display(),
            stderr_path.display()
        );
    }
}

/// Logged once by a soak build when it starts its stress rounds.
const SOAK_MARKER: &str = "HARLAN: soak mode";
/// Logged exactly once per boot: seeing one twice means the machine reset
/// and booted again.
const SOAK_ONCE_MARKERS: [&str; 3] = [
    "HARLAN-PHASE2-POST-EXIT-OK",
    "HARLAN-PHASE0-BOOT-OK",
    SOAK_MARKER,
];
/// Written by the kernel's panic handler and exception handlers.
const SOAK_TROUBLE: [&str; 4] = ["PANIC", "HARLAN: #", "unhandled exception", "NMI received"];

fn soak_test(
    root: &Path,
    duration: Duration,
    min_ticks: u64,
    min_heap_cycles: u64,
    memory: &str,
) -> Result<()> {
    build(root, &["soak"])?;
    let mut cfg = prepare_qemu_config(root, true, false, memory)?;
    let target = root.join("target");
    let log_path = target.join("soak-test.log");
    let qemu_log_path = target.join("soak-test-qemu.log");
    let stderr_path = target.join("soak-test-qemu-stderr.log");
    for path in [&log_path, &qemu_log_path] {
        if path.exists() {
            fs::remove_file(path).with_context(|| format!("failed to clear {}", path.display()))?;
        }
    }
    cfg.debugcon_log = Some(log_path.clone());
    cfg.qemu_log = Some(qemu_log_path.clone());
    let args = build_qemu_args(&cfg);
    let stderr_file = fs::File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let mut child = Command::new(qemu_binary())
        .args(&args)
        .stdout(Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .with_context(|| format!("failed to launch {}", qemu_binary()))?;
    println!("soak-test: running for {duration:?}");

    let start = Instant::now();
    // QEMU logs its own resets while creating the machine, and how many
    // depends on the QEMU version. Counting them the moment the kernel is
    // up makes any later one stand out without hard-coding that number.
    let mut resets_at_boot = None;
    let mut early_exit = None;
    let mut last_ticks = None;
    let mut last_heap_cycles = None;
    let mut ticks_changed_at = start;
    let mut heap_changed_at = start;
    while start.elapsed() < duration {
        if let Some(status) = child.try_wait().context("failed to poll qemu")? {
            early_exit = Some((status, start.elapsed()));
            break;
        }
        if let Ok(log) = fs::read_to_string(&log_path) {
            if resets_at_boot.is_none() && log.contains(SOAK_MARKER) {
                resets_at_boot = Some(count_resets(
                    &fs::read_to_string(&qemu_log_path).unwrap_or_default(),
                ));
            }
            refresh_progress(latest_ticks(&log), &mut last_ticks, &mut ticks_changed_at);
            refresh_progress(
                latest_heap_cycles(&log),
                &mut last_heap_cycles,
                &mut heap_changed_at,
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let _ = child.kill();
    let _ = child.wait();

    let debugcon = fs::read_to_string(&log_path).unwrap_or_default();
    let qemu_log = fs::read_to_string(&qemu_log_path).unwrap_or_default();
    let baseline = resets_at_boot.unwrap_or_else(|| count_resets(&qemu_log));
    const MAX_PROGRESS_SILENCE: Duration = Duration::from_secs(10);
    let freshness = ProgressFreshness {
        ticks_stalled: last_ticks.is_some() && ticks_changed_at.elapsed() > MAX_PROGRESS_SILENCE,
        heap_stalled: last_heap_cycles.is_some()
            && heap_changed_at.elapsed() > MAX_PROGRESS_SILENCE,
    };
    let verdict = analyze_soak(
        &debugcon,
        &qemu_log,
        baseline,
        min_ticks,
        min_heap_cycles,
        freshness,
    );
    let mut problems = verdict.as_ref().err().cloned().unwrap_or_default();
    if let Some((status, after)) = early_exit {
        problems.insert(0, format!("QEMU exited after {after:?} ({status})"));
    }
    match verdict {
        Ok(summary) if problems.is_empty() => {
            println!(
                "soak-test: PASS after {duration:?}: {} ticks, {} heap cycles in {} rounds, \
                 {baseline} CPU reset(s) at machine start and none after",
                summary.ticks, summary.heap_cycles, summary.rounds
            );
            Ok(())
        }
        _ => bail!(
            "soak-test: FAIL\n  - {}\n(see {}, {} and {})",
            problems.join("\n  - "),
            log_path.display(),
            qemu_log_path.display(),
            stderr_path.display()
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SoakSummary {
    ticks: u64,
    heap_cycles: u64,
    rounds: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct ProgressFreshness {
    ticks_stalled: bool,
    heap_stalled: bool,
}

/// Judges a finished soak run from its debugcon log and QEMU's `-d
/// cpu_reset` log; `resets_at_boot` is how many resets QEMU had logged
/// when the kernel's soak marker appeared. Pure, so it is unit-tested.
fn analyze_soak(
    debugcon: &str,
    qemu_log: &str,
    resets_at_boot: usize,
    min_ticks: u64,
    min_heap_cycles: u64,
    freshness: ProgressFreshness,
) -> Result<SoakSummary, Vec<String>> {
    let mut problems = Vec::new();
    for line in debugcon.lines() {
        if SOAK_TROUBLE.iter().any(|sign| line.contains(sign)) {
            problems.push(format!("trouble in the log: {}", line.trim()));
        }
    }
    for marker in SOAK_ONCE_MARKERS {
        let seen = debugcon.matches(marker).count();
        if seen != 1 {
            problems.push(format!(
                "{marker:?} logged {seen} time(s), expected exactly once"
            ));
        }
    }

    let ticks: Vec<u64> = debugcon
        .lines()
        .filter_map(|line| leading_number(line.split_once("HARLAN: ticks=")?.1))
        .collect();
    if ticks.windows(2).any(|pair| pair[1] <= pair[0]) {
        problems.push("timer ticks went backwards or stalled".to_string());
    }
    let last_ticks = ticks.last().copied().unwrap_or(0);
    if last_ticks < min_ticks {
        problems.push(format!(
            "only {last_ticks} timer ticks, minimum {min_ticks}"
        ));
    }

    let rounds: Vec<(u64, u64)> = debugcon
        .lines()
        .filter_map(|line| {
            let (round, rest) = line.split_once("HARLAN: soak round ")?.1.split_once(": ")?;
            Some((round.parse().ok()?, leading_number(rest)?))
        })
        .collect();
    if rounds
        .iter()
        .enumerate()
        .any(|(i, &(round, _))| round != i as u64 + 1)
    {
        problems.push("soak rounds are not consecutive".to_string());
    }
    let (last_round, heap_cycles) = rounds.last().copied().unwrap_or((0, 0));
    if heap_cycles < min_heap_cycles {
        problems.push(format!(
            "only {heap_cycles} heap cycles, minimum {min_heap_cycles}"
        ));
    }
    if freshness.ticks_stalled {
        problems.push("timer made no progress during the final 10 seconds".to_string());
    }
    if freshness.heap_stalled {
        problems.push("heap made no progress during the final 10 seconds".to_string());
    }

    let resets = count_resets(qemu_log);
    if resets > resets_at_boot {
        problems.push(format!(
            "{} CPU reset(s) after the kernel booted",
            resets - resets_at_boot
        ));
    }

    if problems.is_empty() {
        Ok(SoakSummary {
            ticks: last_ticks,
            heap_cycles,
            rounds: last_round,
        })
    } else {
        Err(problems)
    }
}

/// QEMU (11.1, checked) starts every `-d cpu_reset` dump with this line.
fn count_resets(qemu_log: &str) -> usize {
    qemu_log
        .lines()
        .filter(|line| line.starts_with("CPU Reset"))
        .count()
}

fn leading_number(text: &str) -> Option<u64> {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    text[..digits].parse().ok()
}

fn latest_ticks(log: &str) -> Option<u64> {
    log.lines()
        .filter_map(|line| leading_number(line.split_once("HARLAN: ticks=")?.1))
        .next_back()
}

fn latest_heap_cycles(log: &str) -> Option<u64> {
    log.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("HARLAN: soak round ")?.1.split_once(": ")?;
            leading_number(rest)
        })
        .next_back()
}

fn refresh_progress(value: Option<u64>, previous: &mut Option<u64>, changed_at: &mut Instant) {
    if value.is_some() && value != *previous {
        *previous = value;
        *changed_at = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> QemuConfig {
        QemuConfig {
            ovmf_code: PathBuf::from("target/ovmf/x64/code.fd"),
            ovmf_vars: PathBuf::from("target/ovmf-vars.fd"),
            esp_dir: PathBuf::from("target/esp"),
            disk: PathBuf::from("target/disk.img"),
            memory: "256M".to_string(),
            headless: false,
            debug_stub: false,
            debugcon_log: None,
            qemu_log: None,
        }
    }

    #[test]
    fn qemu_args_always_disable_networking() {
        let args = build_qemu_args(&sample_config());
        assert!(
            args.windows(2).any(|w| w == ["-net", "none"]),
            "expected -net none in {args:?}"
        );
    }

    /// The disk has to arrive as a virtio device and the drive it names
    /// has to be the one the device is given, or QEMU starts without it
    /// and the kernel finds nothing.
    #[test]
    fn qemu_args_attach_the_disk_to_a_virtio_device() {
        let args = build_qemu_args(&sample_config());
        let drive = args
            .iter()
            .find(|a| a.contains("disk.img"))
            .expect("the disk is attached");
        assert!(drive.contains("id=harlan-disk"), "{drive}");
        assert!(
            drive.contains("if=none"),
            "not on a bus of its own: {drive}"
        );
        let device = args
            .iter()
            .find(|a| a.starts_with("virtio-blk-pci"))
            .expect("the device is there");
        assert!(device.contains("drive=harlan-disk"), "{device}");
        assert!(
            device.contains("disable-legacy=on"),
            "modern only, so a driver cannot work by the old path: {device}"
        );
        // And the ESP is still what the firmware boots from.
        assert!(args.iter().any(|a| a.contains("fat:rw:")));
    }

    #[test]
    fn qemu_args_headless_adds_display_none() {
        let mut cfg = sample_config();
        cfg.headless = true;
        let args = build_qemu_args(&cfg);
        assert!(args.windows(2).any(|w| w == ["-display", "none"]));

        cfg.headless = false;
        let args = build_qemu_args(&cfg);
        assert!(!args.windows(2).any(|w| w == ["-display", "none"]));
    }

    #[test]
    fn qemu_args_debug_stub_adds_gdbstub_flags() {
        let mut cfg = sample_config();
        cfg.debug_stub = true;
        let args = build_qemu_args(&cfg);
        assert!(args.contains(&"-s".to_string()));
        assert!(args.contains(&"-S".to_string()));
    }

    #[test]
    fn features_reach_both_crates() {
        for command in build_commands(&["heap-stress", "soak"]) {
            assert!(
                command
                    .windows(2)
                    .any(|w| w == ["--features", "heap-stress,soak"]),
                "{command:?}"
            );
        }
        for command in build_commands(&[]) {
            assert!(
                !command.iter().any(|arg| arg == "--features"),
                "{command:?}"
            );
        }
    }

    #[test]
    fn qemu_args_carry_the_requested_memory_size() {
        let mut cfg = sample_config();
        assert!(
            build_qemu_args(&cfg)
                .windows(2)
                .any(|w| w == ["-m", "256M"])
        );
        cfg.memory = "1G".to_string();
        assert!(build_qemu_args(&cfg).windows(2).any(|w| w == ["-m", "1G"]));
    }

    #[test]
    fn qemu_args_route_qemu_diagnostics_when_configured() {
        let mut cfg = sample_config();
        assert!(!build_qemu_args(&cfg).contains(&"-d".to_string()));
        cfg.qemu_log = Some(PathBuf::from("target/soak-test-qemu.log"));
        let args = build_qemu_args(&cfg);
        assert!(
            args.windows(2)
                .any(|w| w == ["-d", "guest_errors,cpu_reset"])
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-D" && w[1].ends_with("soak-test-qemu.log"))
        );
    }

    /// A healthy soak log as the kernel writes it (trimmed).
    fn healthy_soak_log() -> String {
        let mut log = String::from(concat!(
            "[ INFO]: boot\\src\\main.rs@040: HARLAN-PHASE2-POST-EXIT-OK\n",
            "[ INFO]: kernel\\src\\lib.rs@066: HARLAN-PHASE0-BOOT-OK\n",
            "[ INFO]: kernel\\src\\memory\\heap.rs@1: HARLAN: soak mode: heap stress rounds\n",
        ));
        for i in 1..=3u64 {
            log += &format!(
                "[ INFO]: heap.rs@2: HARLAN: soak round {i}: {} heap cycles, 0 corruption\n",
                i * 20_000
            );
            log += &format!("[ INFO]: interrupts.rs@266: HARLAN: ticks={}\n", i * 100);
        }
        log
    }

    const TWO_RESETS: &str = "CPU Reset (CPU 0)\nEAX=00000000\nCPU Reset (CPU 0)\nEAX=00000000\n";

    fn problems(log: &str, qemu_log: &str, min_ticks: u64, min_heap: u64) -> Vec<String> {
        analyze_soak(
            log,
            qemu_log,
            2,
            min_ticks,
            min_heap,
            ProgressFreshness::default(),
        )
        .unwrap_err()
    }

    #[test]
    fn a_healthy_soak_passes_with_its_numbers() {
        let summary = analyze_soak(
            &healthy_soak_log(),
            TWO_RESETS,
            2,
            300,
            60_000,
            ProgressFreshness::default(),
        )
        .unwrap();
        assert_eq!(
            summary,
            SoakSummary {
                ticks: 300,
                heap_cycles: 60_000,
                rounds: 3
            }
        );
    }

    #[test]
    fn too_little_progress_fails() {
        let found = problems(&healthy_soak_log(), TWO_RESETS, 301, 60_001);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn progress_that_stopped_before_the_end_fails() {
        // Each counter on its own, so neither branch can be dropped
        // without a test noticing.
        for (freshness, expected) in [
            (
                ProgressFreshness {
                    ticks_stalled: false,
                    heap_stalled: true,
                },
                "heap made no progress during the final 10 seconds",
            ),
            (
                ProgressFreshness {
                    ticks_stalled: true,
                    heap_stalled: false,
                },
                "timer made no progress during the final 10 seconds",
            ),
        ] {
            let found = analyze_soak(&healthy_soak_log(), TWO_RESETS, 2, 300, 60_000, freshness)
                .unwrap_err();
            assert_eq!(found, [expected]);
        }
    }

    #[test]
    fn a_panic_or_an_exception_fails() {
        for trouble in [
            "[ERROR]: boot\\src\\panic.rs@027: HARLAN PANIC: panicked at heap.rs",
            "[ERROR]: interrupts.rs@281: HARLAN: #GP error_code=0x0 at rip=0x1",
        ] {
            let log = healthy_soak_log() + trouble + "\n";
            let found = problems(&log, TWO_RESETS, 0, 0);
            assert!(found[0].contains("trouble"), "{found:?}");
        }
    }

    #[test]
    fn a_second_boot_fails() {
        let log = healthy_soak_log() + &healthy_soak_log();
        let found = problems(&log, TWO_RESETS, 0, 0);
        assert!(
            found.iter().any(|p| p.contains("logged 2 time(s)")),
            "{found:?}"
        );
    }

    #[test]
    fn a_missing_boot_fails() {
        let found = problems("", TWO_RESETS, 0, 0);
        assert!(
            found.iter().any(|p| p.contains("logged 0 time(s)")),
            "{found:?}"
        );
    }

    #[test]
    fn stalled_ticks_or_skipped_rounds_fail() {
        let log = healthy_soak_log().replace("ticks=300", "ticks=200");
        assert!(problems(&log, TWO_RESETS, 0, 0)[0].contains("ticks"));
        let log = healthy_soak_log().replace("soak round 2:", "soak round 5:");
        assert!(problems(&log, TWO_RESETS, 0, 0)[0].contains("consecutive"));
    }

    #[test]
    fn a_cpu_reset_after_boot_fails() {
        let qemu_log = format!("{TWO_RESETS}CPU Reset (CPU 0)\n");
        let found = problems(&healthy_soak_log(), &qemu_log, 0, 0);
        assert_eq!(found, ["1 CPU reset(s) after the kernel booted"]);
    }

    #[test]
    fn qemu_args_routes_debugcon_to_file_when_configured() {
        let mut cfg = sample_config();
        cfg.debugcon_log = Some(PathBuf::from("target/boot-test.log"));
        let args = build_qemu_args(&cfg);
        let idx = args.iter().position(|a| a == "-debugcon").unwrap();
        assert!(args[idx + 1].starts_with("file:"));
    }
}
