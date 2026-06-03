//! Application des changements Virtumfile sur un lab existant (`virtum apply`).

use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

pub fn apply(file: Option<PathBuf>) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(crate::lab_diff::apply_lab(file))
}
