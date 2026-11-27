//! Display names from the meeting detection registry.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// The registry is also read by the detector. Keep the names in its data file.
const BUILTIN_RULES: &str = include_str!("../../broscribe-detect/src/detect.toml");

#[derive(Deserialize)]
struct Registry {
    #[serde(default)]
    apps: Vec<App>,
}

#[derive(Deserialize)]
struct App {
    id: String,
    name: String,
}

pub fn names(data_dir: &Path) -> BTreeMap<String, String> {
    let builtin: Registry = toml::from_str(BUILTIN_RULES).expect("built-in detector names");
    let overrides = std::fs::read_to_string(data_dir.join("detect.toml"))
        .ok()
        .and_then(|text| toml::from_str::<Registry>(&text).ok());
    builtin
        .apps
        .into_iter()
        .chain(overrides.into_iter().flat_map(|registry| registry.apps))
        .map(|app| (app.id.to_ascii_lowercase(), app.name))
        .collect()
}
