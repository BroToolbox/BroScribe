use std::path::{Path, PathBuf};

pub(crate) fn sdk_resource_compiler(sdk_root: &Path) -> Option<PathBuf> {
    let bin = sdk_root.join("bin");
    let newest = std::fs::read_dir(&bin)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let version: Vec<u32> = name
                .to_str()?
                .split('.')
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            let compiler = entry.path().join("x64").join("rc.exe");
            (version.len() == 4 && compiler.is_file()).then_some((version, compiler))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, compiler)| compiler);
    newest.or_else(|| {
        let compiler = bin.join("x64").join("rc.exe");
        compiler.is_file().then_some(compiler)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_discovery_uses_the_newest_version_with_an_x64_resource_compiler() {
        let root = std::env::temp_dir().join(format!("broscribe-rc-test-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let bin = root.join("bin");
        for (version, arch) in [
            ("10.0.9.0", "x64"),
            ("10.0.10.0", "x64"),
            ("10.0.11.0", "x86"),
            ("preview", "x64"),
        ] {
            let tools = bin.join(version).join(arch);
            std::fs::create_dir_all(&tools).unwrap();
            std::fs::write(tools.join("rc.exe"), []).unwrap();
        }
        let found = sdk_resource_compiler(&root);
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(found, Some(bin.join("10.0.10.0/x64/rc.exe")));
    }
}
