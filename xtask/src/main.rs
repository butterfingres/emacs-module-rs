use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use xshell::{Shell, cmd};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build all crates and copy module artifacts into the target directory
    Build {
        #[arg(long)]
        release: bool,
    },
    /// Run integration tests (requires Emacs installed)
    Test {
        /// Re-run on file changes (requires cargo-watch)
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        release: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build { release } => build(release),
        Command::Test { watch, release } => test(watch, release),
    }
}

fn project_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is xtask/; the workspace root is one level up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ must have a parent directory")
        .to_owned()
}

fn ext() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(windows) {
        "dll"
    } else {
        "so"
    }
}

// On Windows, Cargo omits the "lib" prefix for cdylib outputs (e.g. foo.dll not libfoo.dll).
fn lib_prefix() -> &'static str {
    if cfg!(windows) { "" } else { "lib" }
}

/// Resolves `name` to a native Windows path by searching `PATH`.
///
/// `std::process::Command` does PATH resolution internally but does not expose the
/// resolved path. On Windows the PATH may also contain MSYS2-style entries (e.g.
/// `/c/Users/...`) alongside native ones (e.g. `C:\Users\...`); only native entries
/// are searched because MSYS2 paths are not usable by Windows tools like `objdump`.
fn resolve_in_path(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    // Only treat as pre-resolved if it has a Windows drive prefix (e.g. "C:\...").
    // Paths starting with "/" look absolute in MSYS2 but are NOT on native Windows.
    if p.has_root() && matches!(p.components().next(), Some(std::path::Component::Prefix(_))) {
        return Some(p.to_owned());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        // Skip MSYS2-style entries (e.g. "/c/...") — native Windows entries start with
        // a drive letter ("C:\...") or a UNC prefix ("\\server\...").
        let dir_s = dir.to_string_lossy();
        let is_native = dir_s.starts_with("\\\\")
            || (dir_s.chars().next().map_or(false, |c| c.is_ascii_alphabetic())
                && dir_s.as_bytes().get(1) == Some(&b':'));
        if !is_native {
            continue;
        }
        // On Windows prefer the .exe version so we get the PE binary, not a script/shim.
        for candidate in [dir.join(format!("{name}.exe")), dir.join(name)] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// Print the DLL imports of `path` by parsing `objdump -p` output. Best-effort.
fn print_dll_imports(sh: &Shell, path: &Path) {
    if let Ok(out) = cmd!(sh, "objdump -p {path}").output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.contains("DLL Name") {
                println!("{line}");
            }
        }
    }
}

fn build(release: bool) -> Result<()> {
    let root = project_root();
    let sh = Shell::new()?;
    sh.change_dir(&root);

    let profile = if release { "release" } else { "debug" };
    let release_flag: &[&str] = if release { &["--release"] } else { &[] };

    cmd!(sh, "cargo build --workspace --exclude xtask {release_flag...}").run()?;

    let target = root.join("target").join(profile);
    let ext = ext();
    let prefix = lib_prefix();

    let copies = [
        (format!("{prefix}emacs_rs_module.{ext}"), format!("rs-module.{ext}")),
        (format!("{prefix}test_module.{ext}"), format!("t.{ext}")),
        (format!("{prefix}test_module_28.{ext}"), format!("t28.{ext}")),
    ];
    for (src, dst) in &copies {
        sh.copy_file(target.join(src), target.join(dst))?;
    }

    // Best-effort: show GCC version for build environment info.
    println!("=== Build environment ===");
    let _ = cmd!(sh, "gcc --version").run();
    println!("=========================");

    // On Windows, show which CRT (msvcrt.dll vs ucrtbase.dll) each module imports.
    if cfg!(windows) {
        println!("=== CRT imports for built modules ===");
        for name in ["t.dll", "t28.dll", "rs-module.dll"] {
            let dll = target.join(name);
            println!("--- {name} ---");
            print_dll_imports(&sh, &dll);
        }
        println!("=====================================");
    }

    Ok(())
}

fn test(watch: bool, release: bool) -> Result<()> {
    let root = project_root();
    let sh = Shell::new()?;
    sh.change_dir(&root);

    if watch {
        // cargo-watch doesn't support passing flags through -s easily with spaces,
        // so build and test commands are kept as simple strings.
        let suffix = if release { " --release" } else { "" };
        let build_cmd = format!("cargo xtask build{suffix}");
        let test_cmd = format!("cargo xtask test{suffix}");
        return cmd!(sh, "cargo watch -s {build_cmd} -s {test_cmd}").run().map_err(Into::into);
    }

    let profile = if release { "release" } else { "debug" };
    let target = root.join("target").join(profile);

    // Respect a custom Emacs binary if set in the environment.
    let emacs = std::env::var("EMACS").unwrap_or_else(|_| "emacs".to_string());

    cmd!(sh, "{emacs} --version").run()?;

    // On Windows, show which CRT emacs.exe links against (msvcrt.dll vs ucrtbase.dll).
    // The fd returned by open_channel lives in Emacs's CRT fd table; our modules must
    // use the same CRT, or they will crash.
    if cfg!(windows) {
        if let Some(emacs_path) = resolve_in_path(&emacs) {
            println!("=== Emacs binary: {} ===", emacs_path.display());
            print_dll_imports(&sh, &emacs_path);
            println!("===================================");
        }
    }

    // These env vars are read by the Lisp test helpers (e.g. t/run-in-sub-process uses
    // PROJECT_ROOT and MODULE_DIR to invoke emacs directly in a subprocess).
    // Propagate EMACS so subprocesses spawned by Lisp tests use the same binary.
    sh.set_var("PROJECT_ROOT", &root);
    sh.set_var("MODULE_DIR", &target);
    sh.set_var("EMACS_MODULE_RS_DEBUG", "1");
    sh.set_var("EMACS", &emacs);

    println!("Testing test-module");
    let main_el = root.join("test-module/tests/main.el");
    cmd!(sh, "{emacs} -Q -batch --directory {target} -l ert -l {main_el} -f ert-run-tests-batch-and-exit").run()?;

    println!("Testing test-module-28");
    let main_el_28 = root.join("test-module-28/tests/main.el");
    cmd!(sh, "{emacs} -Q -batch --directory {target} -l ert -l {main_el_28} -f ert-run-tests-batch-and-exit").run()?;

    Ok(())
}
