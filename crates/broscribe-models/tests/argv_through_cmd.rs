//! Windows: an agent CLI's argv survives an npm-style `.cmd` shim exactly
//! (design §8.4). Rust runs a `.cmd` through cmd.exe and quotes for it; the
//! CLI behind the shim (node) parses its argv with the C runtime rules. A
//! `.cmd` in a folder whose name has a space starts this very binary, which
//! writes the argv it received as JSON; it must equal what was passed.
//!
//! `harness = false`: `main` is the argv dump when its first argument is
//! [`DUMP`] (chosen by argv, never the environment, which agent-CLI children
//! get allow-listed), and the test otherwise. The test is Windows-only;
//! elsewhere it reports that it was skipped.

use std::ffi::OsStr;

/// First argument that turns this binary into the argv dump:
/// `--argv-dump <out-file> <args…>`.
const DUMP: &str = "--argv-dump";

fn main() {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() == Some(OsStr::new(DUMP)) {
        let out = args.next().expect("an output file after --argv-dump");
        let received: Vec<String> = args
            .map(|a| a.into_string().expect("a UTF-8 argument"))
            .collect();
        std::fs::write(out, serde_json::to_vec(&received).expect("JSON"))
            .expect("writing the argv dump");
        return;
    }
    #[cfg(windows)]
    {
        windows::argv_through_a_cmd_shim_is_exact();
        windows::find_takes_the_cmd_shim_over_the_bare_one();
        println!("argv_through_cmd: 2 Windows tests passed");
    }
    #[cfg(not(windows))]
    println!("argv_through_cmd: Windows-only tests skipped on this OS");
}

#[cfg(windows)]
mod windows {
    use std::path::Path;
    use std::time::Duration;

    use broscribe_models::ExecutionContext;
    use broscribe_models::llm::SearchPaths;
    use broscribe_models::llm::enrichment_schema;
    use broscribe_models::llm::process::{RunSpec, Sandbox, child_env_with, run};
    use broscribe_platform_native::NativePlatform;

    use super::DUMP;

    /// A temp folder whose name has a space, holding `fake.cmd`, which
    /// starts this binary as the argv dump writing `out.json` beside itself.
    fn shim_dir() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("broscribe argv ")
            .tempdir()
            .expect("temp dir");
        let exe = std::env::current_exe().expect("this test binary");
        let script = format!("@\"{}\" {DUMP} \"%~dp0out.json\" %*\r\n", exe.display());
        std::fs::write(dir.path().join("fake.cmd"), script).expect("writing fake.cmd");
        dir
    }

    pub fn argv_through_a_cmd_shim_is_exact() {
        let dir = shim_dir();
        let shim = dir.path().join("fake.cmd");
        let schema = serde_json::to_string(&enrichment_schema()).expect("schema JSON");
        let args: Vec<String> = vec![
            schema,
            String::new(),
            r"C:\path with space\x.txt".to_owned(),
            "--flag".to_owned(),
        ];
        let env = child_env_with(std::env::vars_os(), &[], true);
        assert!(
            env.iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("SystemRoot")),
            "SystemRoot is passed to the shim"
        );
        let execution = ExecutionContext::new(
            NativePlatform::process_tree_control(),
            NativePlatform::trust_store(),
        );
        let sandbox = Sandbox::new().expect("sandbox");
        let out = run(
            &RunSpec {
                program: &shim,
                args: &args,
                cwd: sandbox.path(),
                env: &env,
                stdin: None,
                timeout: Duration::from_secs(60),
            },
            &execution,
        )
        .expect("running fake.cmd");
        assert!(
            out.status.success(),
            "fake.cmd failed ({}): {}",
            out.status,
            out.stderr_head
        );
        let dumped = std::fs::read(dir.path().join("out.json")).expect("the argv dump");
        let received: Vec<String> = serde_json::from_slice(&dumped).expect("argv JSON");
        assert_eq!(received, args);
    }

    pub fn find_takes_the_cmd_shim_over_the_bare_one() {
        let dir = shim_dir();
        // npm writes an extension-less `sh` shim beside the `.cmd` one.
        std::fs::write(dir.path().join("fake"), "#!/bin/sh\n").expect("writing fake");
        let found = SearchPaths::new(vec![dir.path().to_owned()]).find("fake");
        let found = found.expect("fake.cmd found");
        assert!(
            found
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("cmd")),
            "{}",
            found.display()
        );
        assert_eq!(found.parent(), Some(Path::new(dir.path())));
    }
}
