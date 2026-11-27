//! Shared persisted install fence. Reading never clears an ended hold or
//! creates/locks a directory; offline control must prove the agent absent first.

use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::InstallHold;

const FILE: &str = "install-admission.json";
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
pub struct Persisted {
    pub last_fence: u64,
    #[serde(default)]
    pub held: Option<InstallHold>,
}

impl Persisted {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("run").join(FILE)
    }

    /// Missing is inactive; damaged, nonregular, symlink or oversized records
    /// are errors, never a fresh fence. Contents are never included in errors.
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(data_dir);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io_at(&path, e)),
        };
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(Error::Schema(
                "install admission record is not a bounded regular file".into(),
            ));
        }
        let file = File::open(&path).map_err(|e| Error::io_at(&path, e))?;
        if !file
            .metadata()
            .map_err(|e| Error::io_at(&path, e))?
            .is_file()
        {
            return Err(Error::Schema(
                "install admission record is not a regular file".into(),
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::io_at(&path, e))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::Schema(
                "install admission record exceeds its size limit".into(),
            ));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::Schema("install admission record is invalid".into()))
    }

    /// With the agent proven absent, connection-bound holds have ended.
    pub fn has_durable_hold(&self) -> bool {
        self.held.is_some_and(|hold| hold.durable)
    }
}
