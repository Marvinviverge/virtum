use std::path::Path;
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

/// Runs a program with structured arguments (no shell).
pub fn run_command(program: &str, args: &[&str]) -> Result<Output> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("Impossible d'exécuter {program}"))?;

    if output.status.success() {
        Ok(output)
    } else {
        let stderr = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stderr));
        let stdout = crate::output::sanitize_terminal(&String::from_utf8_lossy(&output.stdout));
        bail!(
            "{program} a échoué (code {:?}).\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        );
    }
}

pub fn path_arg(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("Chemin non UTF-8 (inattendu sur Linux): {}", path.display()))
}
