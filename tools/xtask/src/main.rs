//! Build/run/test automation for HARLAN OS. See ROADMAP.md Fase 0: the goal is
//! a single command that builds and boots the image in QEMU.

use std::{
    fs,
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
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = workspace_root();

    match cli.command {
        XtaskCommand::Build => build(&root),
        XtaskCommand::Run => run(&root),
        XtaskCommand::Test => test(&root),
        XtaskCommand::FmtLint { fix } => fmt_lint(&root, fix),
        XtaskCommand::Debug => debug(&root),
        XtaskCommand::BootTest {
            timeout_secs,
            marker,
            repeat,
        } => boot_test(&root, Duration::from_secs(timeout_secs), &marker, repeat),
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

fn build(root: &Path) -> Result<()> {
    run_cargo(
        root,
        &["build", "-p", "harlan-kernel", "--target", KERNEL_TARGET],
    )?;
    run_cargo(
        root,
        &["build", "-p", "harlan-boot", "--target", UEFI_TARGET],
    )?;
    assemble_esp(root)
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
    headless: bool,
    debug_stub: bool,
    debugcon_log: Option<PathBuf>,
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
        "256M".to_string(),
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

    args
}

fn qemu_binary() -> &'static str {
    "qemu-system-x86_64"
}

fn prepare_qemu_config(root: &Path, headless: bool, debug_stub: bool) -> Result<QemuConfig> {
    let (ovmf_code, ovmf_vars_template) = fetch_ovmf(root)?;
    let ovmf_vars = prepare_vars_copy(root, &ovmf_vars_template)?;
    Ok(QemuConfig {
        ovmf_code,
        ovmf_vars,
        esp_dir: root.join("target").join("esp"),
        headless,
        debug_stub,
        debugcon_log: None,
    })
}

fn run(root: &Path) -> Result<()> {
    build(root)?;
    let cfg = prepare_qemu_config(root, false, false)?;
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
    build(root)?;
    let cfg = prepare_qemu_config(root, false, true)?;
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

fn boot_test(root: &Path, timeout: Duration, marker: &str, repeat: u32) -> Result<()> {
    build(root)?;
    for attempt in 1..=repeat {
        boot_test_once(root, timeout, marker)
            .with_context(|| format!("boot-test attempt {attempt}/{repeat} failed"))?;
    }
    println!("boot-test: {repeat}/{repeat} consecutive successful boots (marker {marker:?})");
    Ok(())
}

fn boot_test_once(root: &Path, timeout: Duration, marker: &str) -> Result<()> {
    let mut cfg = prepare_qemu_config(root, true, false)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> QemuConfig {
        QemuConfig {
            ovmf_code: PathBuf::from("target/ovmf/x64/code.fd"),
            ovmf_vars: PathBuf::from("target/ovmf-vars.fd"),
            esp_dir: PathBuf::from("target/esp"),
            headless: false,
            debug_stub: false,
            debugcon_log: None,
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
    fn qemu_args_routes_debugcon_to_file_when_configured() {
        let mut cfg = sample_config();
        cfg.debugcon_log = Some(PathBuf::from("target/boot-test.log"));
        let args = build_qemu_args(&cfg);
        let idx = args.iter().position(|a| a == "-debugcon").unwrap();
        assert!(args[idx + 1].starts_with("file:"));
    }
}
