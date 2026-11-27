#![cfg(target_os = "linux")]

use broscribe_platform::{
    AppPaths, BrowserLauncher, LibraryWatcher, PcmSink, ProcessProbe, ResourceUsage, Result,
    SystemInfo,
};
use broscribe_platform_linux::{
    LinuxBrowserLauncher, LinuxLibraryWatcher, LinuxPaths, LinuxProcessProbe, LinuxResourceUsage,
    LinuxSystemInfo, decoder_chain,
};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn process_probe_recognizes_this_process_and_refuses_pid_zero() {
    let probe = LinuxProcessProbe;
    assert_eq!(probe.current_pid(), std::process::id());
    assert!(probe.is_alive(probe.current_pid()));
    assert!(!probe.is_alive(0));
}

#[test]
fn browser_rejects_non_web_urls_before_starting_a_helper() {
    let browser = LinuxBrowserLauncher;
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://",
        "https://example.invalid/\n",
    ] {
        assert_eq!(browser.open(url).unwrap_err().code(), "invalid_input");
    }
}

#[test]
fn machine_traits_report_user_paths_and_existing_volume_capacity() {
    let paths = LinuxPaths::new();
    assert!(paths.data_dir().is_absolute());
    assert!(paths.cache_dir().is_absolute());
    assert!(paths.default_library_dir().is_absolute());
    let temporary = tempfile::tempdir().unwrap();
    let system = LinuxSystemInfo;
    assert!(system.cpu_cores() > 0);
    let free = system
        .free_disk_bytes(&temporary.path().join("not-created/child"))
        .unwrap();
    assert!(free > 0);
    let usage = LinuxResourceUsage.self_usage().unwrap();
    assert!(usage.peak_rss_bytes > 0);
    assert!(usage.user_cpu_s >= 0.0 && usage.system_cpu_s >= 0.0);
}

#[test]
fn watcher_reports_a_synthetic_library_database_write() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    std::fs::create_dir(root.join(".index")).unwrap();
    let (sender, receiver) = mpsc::channel();
    let watch = LinuxLibraryWatcher
        .watch(
            root,
            Box::new(move |paths| {
                sender.send(paths).unwrap();
            }),
        )
        .unwrap();
    let database = root.join(".index/broscribe.sqlite");
    std::fs::write(&database, b"synthetic change event, not a database").unwrap();
    let changes = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(changes.contains(&database));
    watch.stop();
}

#[derive(Default)]
struct Pcm {
    rate: u32,
    samples: Vec<f32>,
}
impl PcmSink for Pcm {
    fn format(&mut self, sample_rate: u32) -> Result<()> {
        self.rate = sample_rate;
        Ok(())
    }
    fn samples(&mut self, samples: &[f32]) -> Result<()> {
        self.samples.extend_from_slice(samples);
        Ok(())
    }
}
#[test]
fn decoder_chain_decodes_synthetic_pcm_without_external_binaries() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("synthetic.wav");
    let samples = [0i16, 16384, -16384, 0];
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&44u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16000u32.to_le_bytes());
    wav.extend_from_slice(&32000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&8u32.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(&path, wav).unwrap();
    let mut pcm = Pcm::default();
    let info = decoder_chain().decode(&path, &mut pcm).unwrap();
    assert_eq!(info.sample_rate, 16000);
    assert_eq!(pcm.rate, 16000);
    assert_eq!(pcm.samples, vec![0.0, 0.5, -0.5, 0.0]);
}

#[test]
fn wayland_session_type_disables_titles_without_wayland_display() {
    if std::env::var_os("BROSCRIBE_TEST_WAYLAND").is_some() {
        use broscribe_platform::WindowInspector;
        use broscribe_platform_linux::LinuxWindowInspector;
        assert!(!LinuxWindowInspector::titles_available());
        assert!(matches!(
            LinuxWindowInspector.windows(),
            Err(broscribe_platform::Error::Unsupported(_))
        ));
        return;
    }
    assert!(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "wayland_session_type_disables_titles_without_wayland_display"
            ])
            .env("BROSCRIBE_TEST_WAYLAND", "1")
            .env("XDG_SESSION_TYPE", "wayland")
            .env("DISPLAY", ":synthetic")
            .env_remove("WAYLAND_DISPLAY")
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn missing_x11_display_is_unsupported_but_a_failing_display_is_an_error() {
    if let Some(case) = std::env::var_os("BROSCRIBE_TEST_NO_DISPLAY") {
        use broscribe_platform::WindowInspector;
        use broscribe_platform_linux::LinuxWindowInspector;
        let result = LinuxWindowInspector.windows();
        if case == "absent" {
            assert!(!LinuxWindowInspector::titles_available());
            assert!(
                matches!(result, Err(broscribe_platform::Error::Unsupported(_))),
                "{result:?}"
            );
        } else {
            // A configured display without a server is an operational error.
            assert!(LinuxWindowInspector::titles_available());
            assert!(
                matches!(&result, Err(e) if !matches!(e, broscribe_platform::Error::Unsupported(_))),
                "{result:?}"
            );
        }
        return;
    }
    for (case, display) in [
        ("absent", None),
        ("empty", Some("")),
        ("failing", Some(":4242")),
    ] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "missing_x11_display_is_unsupported_but_a_failing_display_is_an_error",
            ])
            .env(
                "BROSCRIBE_TEST_NO_DISPLAY",
                if case == "failing" {
                    "failing"
                } else {
                    "absent"
                },
            )
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("XDG_SESSION_TYPE")
            .env_remove("DISPLAY");
        if let Some(display) = display {
            child.env("DISPLAY", display);
        }
        assert!(child.status().unwrap().success(), "{case}");
    }
}

#[test]
fn uninstall_retry_environment_pins_config_and_only_an_absolute_runtime() {
    use broscribe_platform::AppPaths;
    use broscribe_platform_linux::LinuxPaths;
    use std::path::Path;

    if let Some(scenario) = std::env::var_os("BROSCRIBE_TEST_RETRY_ENV") {
        let paths = LinuxPaths::new();
        let env: std::collections::BTreeMap<_, _> =
            paths.uninstall_retry_env().into_iter().collect();
        let home = std::env::var_os("HOME").unwrap();
        let home = Path::new(&home);
        let configured = scenario == "absolute";
        let config = home.join(if configured {
            "config with spaces"
        } else {
            ".config"
        });
        assert_eq!(env.get("XDG_CONFIG_HOME").copied(), Some(config.as_path()));
        let runtime = home.join("runtime");
        assert_eq!(
            env.get("XDG_RUNTIME_DIR").copied(),
            configured.then_some(runtime.as_path())
        );
        return;
    }

    let temporary = tempfile::tempdir().unwrap();
    for scenario in ["absolute", "relative", "unset"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "uninstall_retry_environment_pins_config_and_only_an_absolute_runtime",
            ])
            .env_clear()
            .env("HOME", temporary.path())
            .env("BROSCRIBE_TEST_RETRY_ENV", scenario);
        match scenario {
            "absolute" => {
                child.env(
                    "XDG_CONFIG_HOME",
                    temporary.path().join("config with spaces"),
                );
                child.env("XDG_RUNTIME_DIR", temporary.path().join("runtime"));
            }
            "relative" => {
                child.env("XDG_CONFIG_HOME", "relative");
                child.env("XDG_RUNTIME_DIR", "relative");
            }
            _ => {}
        }
        assert!(child.status().unwrap().success(), "{scenario}");
    }
}

#[test]
fn stopping_without_an_installed_systemd_unit_is_a_noop() {
    if std::env::var_os("BROSCRIBE_TEST_STOP").is_some() {
        use broscribe_platform::{Autostart, LoginJob};
        use broscribe_platform_linux::LinuxAutostart;
        let missing = std::path::PathBuf::from("/synthetic/broscribed");
        let autostart = LinuxAutostart::new(missing);
        assert_eq!(
            autostart.login_job(std::path::Path::new("/synthetic/profile")),
            LoginJob::Absent
        );
        autostart.stop().unwrap();
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stopping_without_an_installed_systemd_unit_is_a_noop"
            ])
            .env("BROSCRIBE_TEST_STOP", "1")
            .env("XDG_CONFIG_HOME", temporary.path())
            .env("PATH", temporary.path())
            .status()
            .unwrap()
            .success()
    );
}
