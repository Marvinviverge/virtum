use std::path::Path;
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

use crate::tr;

/// Runs a program with structured arguments (no shell).
pub fn run_command(program: &str, args: &[&str]) -> Result<Output> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| tr!(fr: "Impossible d'exécuter {program}", en: "Cannot run {program}"))?;

    if output.status.success() {
        Ok(output)
    } else {
        let stderr = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stderr));
        let stdout = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stdout));
        bail!(tr!(
            fr: "{program} a échoué (code {:?}).\nstdout: {stdout}\nstderr: {stderr}",
            en: "{program} failed (code {:?}).\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        ));
    }
}

pub fn path_arg(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| {
        let path = path.display();
        tr!(
            fr: "Chemin non UTF-8 (inattendu sur Linux): {path}",
            en: "Non-UTF-8 path (unexpected on Linux): {path}"
        )
    })
}
