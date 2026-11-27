use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

pub fn compile_live_timing_worker(program: &Path) {
    let mut compiler = Command::new("clippy-driver");
    compiler.current_dir(env!("CARGO_MANIFEST_DIR")).args([
        "--edition=2024",
        "-Wclippy::all",
        "-Dwarnings",
        "tests/fixtures/live_timing_worker.rs",
        "-o",
    ]);
    compiler.arg(program);
    if cfg!(target_env = "msvc") {
        // Direct compiler invocations do not inherit Cargo's linker setting.
        // Git for Windows also ships a different executable named link.exe.
        let linker = std::env::var_os("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("BROSCRIBE_MSVC_BIN").map(|dir| Path::new(&dir).join("link.exe"))
            })
            .or_else(|| {
                std::env::var_os("PATH")
                    .as_deref()
                    .and_then(msvc_linker_on_path)
            });
        if let Some(linker) = linker {
            let mut option = std::ffi::OsString::from("linker=");
            option.push(linker);
            compiler.arg("-C").arg(option);
        }
    }
    let output = compiler
        .output()
        .expect("compile live timing fixture with the required Rust toolchain");
    assert!(
        output.status.success(),
        "compile live timing fixture: {}
{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn msvc_linker_on_path(path: &OsStr) -> Option<PathBuf> {
    let tool_dir = std::env::split_paths(path).find(|dir| dir.join("cl.exe").is_file())?;
    let linker = tool_dir.join("link.exe");
    if linker.is_file() {
        std::path::absolute(linker).ok()
    } else {
        None
    }
}

#[test]
fn fixture_finds_msvc_linker_beside_the_compiler() {
    let git = tempfile::tempdir().unwrap();
    let msvc = tempfile::tempdir().unwrap();
    for path in [git.path().join("link.exe"), msvc.path().join("cl.exe")] {
        std::fs::write(path, b"synthetic").unwrap();
    }
    let path = std::env::join_paths([git.path(), msvc.path()]).unwrap();
    assert_eq!(msvc_linker_on_path(&path), None);
    let linker = msvc.path().join("link.exe");
    std::fs::write(&linker, b"synthetic").unwrap();
    assert_eq!(msvc_linker_on_path(&path), Some(linker));
}
