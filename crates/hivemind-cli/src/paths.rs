//! Where hivemind keeps its data (SPEC §4.3).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// `~/.hivemind`, or whatever `--home` / `HIVEMIND_HOME` says.
pub(crate) fn home(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    let dirs = directories::BaseDirs::new()
        .context("could not work out your home directory; pass --home")?;
    Ok(dirs.home_dir().join(".hivemind"))
}
