//! The daemon's named-pipe name: per user (SID) and, like the Unix socket,
//! per data folder, so daemons for different data folders never answer each
//! other's clients.
use std::path::Path;

/// `\\.\pipe\broscribed-<sid>-<key>`, `key` derived from the data folder.
pub(crate) fn pipe_name_for(sid: &str, data_dir: &Path) -> String {
    format!(
        r"\\.\pipe\broscribed-{sid}-{:016x}",
        fnv1a64(&profile_identity(data_dir))
    )
}

/// `\\.\pipe\broscribe-app-<sid>-<key>`: the desktop app's single-instance
/// pipe (SHL-14), per user and data folder like the daemon's.
pub(crate) fn app_pipe_name_for(sid: &str, data_dir: &Path) -> String {
    format!(
        r"\\.\pipe\broscribe-app-{sid}-{:016x}",
        fnv1a64(&profile_identity(data_dir))
    )
}

/// A data folder's identity, shared by the daemon pipe and the secret
/// namespace: its [`broscribe_platform::stable_path`], folded, so every
/// Windows spelling of one folder (case, separators, verbatim prefix,
/// trailing `\`) is one profile, the same before and after it exists.
pub fn profile_identity(data_dir: &Path) -> Vec<u8> {
    recorded_profile_identity(&broscribe_platform::stable_path(data_dir))
}

/// An already resolved profile path from a durable retry record.
pub fn recorded_profile_identity(path: &Path) -> Vec<u8> {
    folded(path).into_bytes()
}

/// Windows paths compare case-insensitively and with either separator; the
/// verbatim `\\?\` prefix `canonicalize` adds is not part of the identity.
fn folded(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = text
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned());
    text.trim_end_matches('\\').to_lowercase()
}

/// FNV-1a: a stable, dependency-free key (not a security boundary; the pipe
/// ACL and the server SID check are).
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names_differ_per_data_folder_and_are_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        std::fs::create_dir(&one).unwrap();
        let sid = "S-1-5-21-1-2-3-1001";
        let name = pipe_name_for(sid, &one);
        assert!(name.starts_with(r"\\.\pipe\broscribed-S-1-5-21-1-2-3-1001-"));
        assert_ne!(name, pipe_name_for(sid, &two));
        assert_eq!(name, pipe_name_for(sid, &one.join(".")));
        assert_eq!(name, pipe_name_for(sid, &two.join("..").join("one")));
        assert_ne!(name, pipe_name_for("S-1-5-21-1-2-3-1002", &one));
        // Created after the name was derived: unchanged.
        let later = pipe_name_for(sid, &two);
        std::fs::create_dir(&two).unwrap();
        assert_eq!(later, pipe_name_for(sid, &two));
    }

    #[test]
    fn the_app_pipe_is_its_own_per_user_and_data_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one");
        let sid = "S-1-5-21-1-2-3-1001";
        let app = app_pipe_name_for(sid, &one);
        assert!(app.starts_with(r"\\.\pipe\broscribe-app-S-1-5-21-1-2-3-1001-"));
        assert_ne!(app, pipe_name_for(sid, &one), "never the daemon's pipe");
        assert_eq!(
            app.rsplit('-').next(),
            pipe_name_for(sid, &one).rsplit('-').next(),
            "the same data-folder key"
        );
        assert_ne!(app, app_pipe_name_for(sid, &tmp.path().join("two")));
        assert_ne!(app, app_pipe_name_for("S-1-5-21-1-2-3-1002", &one));
    }

    #[test]
    fn windows_spellings_fold_to_one_key() {
        assert_eq!(
            folded(Path::new(r"\\?\C:\Users\A\AppData\BroScribe\")),
            folded(Path::new("c:/users/a/appdata/broscribe"))
        );
        assert_eq!(
            folded(Path::new(r"\\?\UNC\server\share\d")),
            r"\\server\share\d"
        );
    }

    #[test]
    fn profile_identity_is_the_folded_stable_path_of_every_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("One");
        let identity = profile_identity(&one);
        assert_eq!(
            identity,
            folded(&broscribe_platform::stable_path(&one)).into_bytes()
        );
        assert_eq!(identity, profile_identity(&one.join(".")));
        assert_eq!(identity, profile_identity(&one.join("x").join("..")));
        assert_ne!(identity, profile_identity(&tmp.path().join("two")));
        #[cfg(target_os = "windows")]
        {
            let upper = one.to_string_lossy().to_uppercase().replace('\\', "/");
            assert_eq!(identity, profile_identity(Path::new(&upper)));
            assert_eq!(
                identity,
                profile_identity(Path::new(&format!(r"{}\", one.display())))
            );
        }
    }
}
