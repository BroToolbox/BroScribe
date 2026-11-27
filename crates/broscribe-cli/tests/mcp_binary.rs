//! The real `broscribe` binary serving MCP on stdio: a JSON-RPC round trip
//! over the process's pipes (the in-process tests use a duplex and cannot see
//! how `main` hands stdout to the server).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

/// Kills the server if the test fails while it still runs.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn binary_answers_initialize_and_tools_list_over_stdio() {
    for mode in [
        broscribe_core::maintenance::Mode::Normal,
        broscribe_core::maintenance::Mode::Maintenance,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        broscribe_core::Library::open(tmp.path().join("lib")).unwrap();
        let index = tmp.path().join("lib/.index");
        if index.exists() {
            std::fs::remove_dir_all(index).unwrap();
        }
        broscribe_core::config::Config::update(&tmp.path().join("data"), |config| {
            config.mcp.enabled = Some(true);
            Ok(())
        })
        .unwrap();
        let change =
            broscribe_core::maintenance::ModeChange::acquire(&tmp.path().join("data")).unwrap();
        change.set(mode).unwrap();
        drop(change);
        let mut child = Reap(
            Command::new(env!("CARGO_BIN_EXE_broscribe"))
                .arg("--library")
                .arg(tmp.path().join("lib"))
                .arg("mcp")
                .env("BROSCRIBE_DATA_DIR", tmp.path().join("data"))
                .env_remove("BROSCRIBE_LIBRARY")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut stdin = child.0.stdin.take().unwrap();
        let stdout = child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let response = |id: u64| -> Value {
            loop {
                let line = rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("no MCP response within 30 s (stdout deadlock?)");
                let v: Value = serde_json::from_str(&line).unwrap();
                if v["id"] == json!(id) {
                    return v;
                }
            }
        };
        let mut send = |v: Value| {
            writeln!(stdin, "{v}").unwrap();
            stdin.flush().unwrap();
        };

        send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }));
        let init = response(1);
        assert_eq!(init["result"]["serverInfo"]["name"], "broscribe", "{init}");

        send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
        let tools = response(2);
        let tools = tools["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        assert!(
            broscribe_core::maintenance::ModeChange::acquire(&tmp.path().join("data")).is_err()
        );
        if mode == broscribe_core::maintenance::Mode::Maintenance {
            send(
                json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_meetings","arguments":{}}}),
            );
            let refused = response(3);
            assert_eq!(refused["result"]["isError"], true, "{refused}");
            assert_eq!(
                refused["result"]["structuredContent"]["error"]["code"], "maintenance",
                "{refused}"
            );
            assert!(!tmp.path().join("lib/.index").exists());
        }

        drop(send);
        drop(stdin);
        let status = child.0.wait().unwrap();
        assert!(status.success(), "{status:?}");
        broscribe_core::maintenance::ModeChange::acquire(&tmp.path().join("data")).unwrap();
    }
}
