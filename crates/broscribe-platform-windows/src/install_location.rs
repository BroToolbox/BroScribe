//! Where a daemon may be registered to start at logon, and the exact Run
//! command line (with the data/library overrides the daemon must keep).
//! Pure policy, unit-tested on every OS.
use std::path::Path;

/// Why `executable` (already canonical) must not be registered, if so.
/// The owner's installation is never moved or rewritten: refusing lets the
/// user install BroScribe somewhere stable first.
pub(crate) fn location_problem(executable: &str, temp_dir: Option<&str>) -> Option<&'static str> {
    let plain = executable
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| {
            executable
                .strip_prefix(r"\\?\")
                .unwrap_or(executable)
                .to_owned()
        });
    if plain.starts_with(r"\\") {
        return Some("BroScribe runs from a network share; install it on this computer first");
    }
    let lower = plain.to_lowercase();
    if let Some(temp) = temp_dir {
        let temp = temp
            .strip_prefix(r"\\?\")
            .unwrap_or(temp)
            .trim_end_matches('\\')
            .to_lowercase();
        if !temp.is_empty() && lower.starts_with(&format!("{temp}\\")) {
            return Some(
                "BroScribe runs from a temporary folder (an unextracted download?); install it first",
            );
        }
    }
    None
}

/// One argument quoted for `CommandLineToArgvW`: a path cannot contain `"`,
/// and backslashes before the closing quote are doubled.
pub(crate) fn quote(argument: &str) -> Option<String> {
    if argument.contains('"') || argument.chars().any(char::is_control) {
        return None;
    }
    let trailing = argument.len() - argument.trim_end_matches('\\').len();
    Some(format!("\"{argument}{}\"", "\\".repeat(trailing)))
}

/// The Run value: the daemon, then the overrides it must keep at logon,
/// since the Run key cannot carry environment variables.
pub(crate) fn run_command(
    daemon: &Path,
    data_dir: Option<&Path>,
    library: Option<&Path>,
) -> Option<String> {
    let text = |path: &Path| path.to_str().and_then(quote);
    let mut command = text(daemon)?;
    for (flag, value) in [("--data-dir", data_dir), ("--library", library)] {
        if let Some(value) = value {
            command.push(' ');
            command.push_str(flag);
            command.push(' ');
            command.push_str(&text(value)?);
        }
    }
    Some(command)
}

/// The pre-rename entry always quoted its executable. Match the installation,
/// allowing old profile arguments and the optional verbatim path prefix.
pub(crate) fn legacy_run_targets(command: &str, daemon: &Path) -> bool {
    let Some((executable, args)) = command.strip_prefix('"').and_then(|s| s.split_once('"')) else {
        return false;
    };
    if !args.is_empty() && !args.starts_with(' ') {
        return false;
    }
    let windows_path = |path: &str| path.strip_prefix(r"\\?\").unwrap_or(path).to_lowercase();
    daemon
        .to_str()
        .is_some_and(|daemon| windows_path(executable) == windows_path(daemon))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn network_and_temporary_locations_are_refused() {
        let temp = Some(r"C:\Users\Synthetic\AppData\Local\Temp");
        assert!(location_problem(r"\\?\C:\Program Files\BroScribe\broscribed.exe", temp).is_none());
        assert!(
            location_problem(
                r"C:\Users\Synthetic\AppData\Local\BroScribe\broscribed.exe",
                temp
            )
            .is_none()
        );
        assert!(location_problem(r"\\?\UNC\server\share\broscribed.exe", temp).is_some());
        assert!(location_problem(r"\\server\share\broscribed.exe", temp).is_some());
        assert!(
            location_problem(
                r"\\?\c:\users\synthetic\appdata\local\temp\Rar$EX00\broscribed.exe",
                temp
            )
            .is_some()
        );
        // A sibling whose name merely starts like the temp folder is fine.
        assert!(
            location_problem(
                r"C:\Users\Synthetic\AppData\Local\TempApps\broscribed.exe",
                temp
            )
            .is_none()
        );
    }

    #[test]
    fn run_commands_keep_overrides_and_quote_safely() {
        let daemon = PathBuf::from(r"C:\Program Files\BroScribe\broscribed.exe");
        assert_eq!(
            run_command(&daemon, None, None).unwrap(),
            r#""C:\Program Files\BroScribe\broscribed.exe""#
        );
        assert_eq!(
            run_command(
                &daemon,
                Some(Path::new(r"D:\Synthetic Data\")),
                Some(Path::new(r"D:\Library"))
            )
            .unwrap(),
            r#""C:\Program Files\BroScribe\broscribed.exe" --data-dir "D:\Synthetic Data\\" --library "D:\Library""#
        );
        assert_eq!(quote("a\"b"), None);
        assert_eq!(quote("line\nbreak"), None);
    }

    #[test]
    fn legacy_run_value_must_target_this_daemon() {
        let daemon = Path::new(r"\\?\C:\Program Files\BroScribe\broscribed.exe");
        assert!(legacy_run_targets(
            r#""c:\program files\broscribe\broscribed.exe" --data-dir "D:\Old Profile""#,
            daemon,
        ));
        for other in [
            r#""C:\Program Files\BroScribe\broscribe-app.exe""#,
            r#""C:\Other\broscribed.exe""#,
            r#""C:\Program Files\BroScribe\broscribed.exe"unrelated"#,
        ] {
            assert!(!legacy_run_targets(other, daemon));
        }
    }
}
