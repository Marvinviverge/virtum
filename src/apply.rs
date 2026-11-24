//! Application des changements Virtumfile sur un lab existant (`virtum apply`).

use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

use crate::tr;

pub fn apply(
    file: Option<PathBuf>,
    insecure_ssh: bool,
    prune: bool,
    yes: bool,
    yes_lan: bool,
) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context(tr!(
            fr: "Impossible de démarrer le runtime Tokio",
            en: "Cannot start the Tokio runtime"
        ))?
        .block_on(crate::lab_diff::apply_lab(
            file,
            insecure_ssh,
            prune,
            yes,
            yes_lan,
        ))
}
