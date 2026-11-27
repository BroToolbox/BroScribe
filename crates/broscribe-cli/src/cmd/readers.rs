//! `readers dump`: the redacted accessibility structure of a running meeting
//! app, for writing reader rules. Only structure, AX roles and hashed
//! identifiers are printed; names, captions and titles never are.
use std::path::PathBuf;

use broscribe_core::{Error, Result};
use broscribe_platform::{AppUiReader, MicUsageMonitor, WindowInspector};
use broscribe_platform_native::NativePlatform;
use clap::{ArgGroup, Args, Subcommand};
use serde_json::{Value, json};

use crate::Ctx;

#[derive(Debug, Subcommand)]
pub(crate) enum ReadersCmd {
    /// Print or save a running app's redacted accessibility structure. Needs
    /// the Accessibility permission; never requests it. Chromium browsers
    /// expose their page structure only while accessibility is enabled.
    Dump(DumpArgs),
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("target").required(true).args(["app", "pid"])))]
pub(crate) struct DumpArgs {
    /// macOS bundle id, Linux application id / binary name or Windows
    /// executable file name of a running app (e.g. com.microsoft.teams2).
    #[arg(long)]
    pub app: Option<String>,
    /// Process ID to read instead of `--app`.
    #[arg(long)]
    pub pid: Option<u32>,
    /// Write the dump to this file instead of standard output.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,
}

pub(super) fn run(ctx: &mut Ctx<'_>, action: ReadersCmd) -> Result<()> {
    match action {
        ReadersCmd::Dump(args) => {
            let reader = NativePlatform::app_ui_reader();
            let monitor = NativePlatform::mic_usage_monitor();
            let inspector = NativePlatform::window_inspector();
            let (value, text) = dump(reader.as_ref(), monitor.as_ref(), inspector.as_ref(), &args)?;
            ctx.emit(value.clone(), |out| match &text {
                Some(text) => out.write_all(text.as_bytes()),
                None => writeln!(
                    out,
                    "Wrote {} redacted nodes of process {} to {}{}",
                    value["nodes"],
                    value["pid"],
                    value["output"].as_str().unwrap_or_default(),
                    match value["incomplete"].as_array() {
                        Some(reasons) if !reasons.is_empty() => format!(
                            " (truncated: {})",
                            reasons
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        _ => String::new(),
                    }
                ),
            })
        }
    }
}

/// The JSON report and, without `--output`, the redacted dump to print.
fn dump(
    reader: &dyn AppUiReader,
    monitor: &dyn MicUsageMonitor,
    inspector: &dyn WindowInspector,
    args: &DumpArgs,
) -> Result<(Value, Option<String>)> {
    let pid = match (args.pid, args.app.as_deref()) {
        (Some(pid), _) => pid,
        (None, Some(app)) => {
            // Either source may be unavailable (no titles on Wayland, no
            // microphone users): the other one still identifies the app.
            let users = monitor.mic_users().unwrap_or_default();
            let windows = inspector.windows().unwrap_or_default();
            broscribe_readers::dump::find_pid(app, &users, &windows).ok_or_else(|| {
                Error::NotFound(format!(
                    "no running process for {app:?}; pass --pid to choose one"
                ))
            })?
        }
        (None, None) => return Err(Error::InvalidInput("pass --app or --pid".into())),
    };
    if !reader.is_trusted() {
        return Err(Error::Unsupported(
            "Accessibility permission is not granted; grant it in the app's setup assistant or the system's accessibility settings".into(),
        ));
    }
    let (text, summary) = broscribe_readers::dump::dump_snapshot(reader, pid)?;
    let mut value = json!({
        "pid": pid,
        "nodes": summary.nodes,
        "truncated": summary.truncated(),
        "incomplete": summary.incomplete.names(),
    });
    if let Some(path) = &args.output {
        broscribe_core::fsutil::write_atomic(path, text.as_bytes())?;
        value["output"] = json!(path);
        Ok((value, None))
    } else {
        value["dump"] = json!(text);
        Ok((value, Some(text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::fakes::{FakeAppUiReader, FakeMicUsageMonitor};
    use broscribe_platform::{AxNode, AxTree, MicUser, WindowInfo};

    struct NoWindows;
    impl WindowInspector for NoWindows {
        fn windows(&self) -> Result<Vec<WindowInfo>> {
            Err(Error::Unsupported("synthetic".into()))
        }
    }

    fn reader() -> FakeAppUiReader {
        let reader = FakeAppUiReader::default();
        reader.set_tree(
            4242,
            AxTree {
                root: AxNode {
                    title: Some("Invented Meeting Title".into()),
                    children: vec![AxNode {
                        value: Some("Invented Person said synthetic words".into()),
                        identifier: Some("caption-1".into()),
                        ..AxNode::new("AXStaticText")
                    }],
                    ..AxNode::new("AXWindow")
                },
                incomplete: Default::default(),
            },
        );
        reader
    }

    #[test]
    fn readers_dump_by_app_prints_structure_without_text() {
        let monitor = FakeMicUsageMonitor::new(vec![MicUser {
            pid: 4242,
            bundle_id: Some("teams.exe".into()),
            responsible_pid: None,
            responsible_bundle_id: None,
        }]);
        let args = DumpArgs {
            app: Some("Teams.EXE".into()),
            pid: None,
            output: None,
        };
        let (value, text) = dump(&reader(), &monitor, &NoWindows, &args).unwrap();
        let text = text.unwrap();
        let json = value.to_string();
        for leaked in ["Invented", "synthetic words", "caption-1"] {
            assert!(!text.contains(leaked) && !json.contains(leaked), "{leaked}");
        }
        assert!(text.contains("AXWindow title[len=22]\n  AXStaticText"));
        assert_eq!(value["pid"], json!(4242));
        assert_eq!(value["nodes"], json!(2));
        assert_eq!(value["truncated"], json!(false));
        assert_eq!(value["incomplete"], json!([]));
    }

    #[test]
    fn readers_dump_names_what_cut_the_snapshot_short() {
        let reader = reader();
        let mut tree = reader.snapshot(4242, Default::default()).unwrap();
        tree.incomplete.read_failed = true;
        tree.incomplete.time_budget = true;
        reader.set_tree(4242, tree);
        let args = DumpArgs {
            app: None,
            pid: Some(4242),
            output: None,
        };
        let (value, text) =
            dump(&reader, &FakeMicUsageMonitor::default(), &NoWindows, &args).unwrap();
        assert_eq!(value["truncated"], json!(true));
        assert_eq!(value["incomplete"], json!(["time_budget", "read_failed"]));
        assert!(
            text.unwrap()
                .starts_with("truncated=true incomplete=time_budget,read_failed\nAXWindow")
        );
    }

    #[test]
    fn readers_dump_writes_file_and_refuses_without_permission_or_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.txt");
        let monitor = FakeMicUsageMonitor::default();
        let args = DumpArgs {
            app: None,
            pid: Some(4242),
            output: Some(path.clone()),
        };
        let (value, text) = dump(&reader(), &monitor, &NoWindows, &args).unwrap();
        assert!(text.is_none() && value.get("dump").is_none());
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("truncated=false\nAXWindow") && !saved.contains("Invented"));

        let missing = DumpArgs {
            app: Some("zoom".into()),
            pid: None,
            output: None,
        };
        assert!(matches!(
            dump(&reader(), &monitor, &NoWindows, &missing),
            Err(Error::NotFound(_))
        ));
        let mut denied = reader();
        denied.denied = true;
        assert!(matches!(
            dump(&denied, &monitor, &NoWindows, &args),
            Err(Error::Unsupported(_))
        ));
    }
}
