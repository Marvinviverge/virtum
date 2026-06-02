pub mod cli;
pub mod cloud_init;
pub mod command;
pub mod config;
pub mod disk;
pub mod doctor;
pub mod images;
pub mod lab;
pub mod libvirt;
pub mod libvirt_access;
pub mod metadata;
pub mod ps;
pub mod ssh;
pub mod test_env;
pub mod virtumfile;
pub mod vm;

use anyhow::{bail, Result};
use clap::Parser;
use cli::{Cli, Commands};

pub fn run() -> Result<()> {
    dispatch(Cli::parse().command)
}

fn dispatch(command: Commands) -> Result<()> {
    match command {
        Commands::Doctor => std::process::exit(doctor::run()),
        Commands::Ps { all } => ps::ps(all)?,
        Commands::Run(args) => vm::run_vm(args)?,
        Commands::Shell { vm } => ssh::shell_to_instance(&vm)?,
        Commands::Start { vms } => run_for_each_vm(&vms, vm::start_vm)?,
        Commands::Stop { vms } => run_for_each_vm(&vms, vm::stop_vm)?,
        Commands::Destroy { vms } => run_for_each_vm(&vms, vm::destroy_vm)?,
        Commands::Up { file } => lab::up(file)?,
        Commands::Down { file, destroy } => lab::down(file, destroy)?,
    };
    Ok(())
}

/// Exécute une opération sur chaque référence VM ; continue après échec, puis résume les erreurs.
fn run_for_each_vm(refs: &[String], op: fn(&str) -> Result<()>) -> Result<()> {
    let mut failures = Vec::new();
    for reference in refs {
        let reference = reference.trim();
        if reference.is_empty() {
            continue;
        }
        if let Err(err) = op(reference) {
            eprintln!("error [{reference}]: {err:#}");
            failures.push(reference.to_string());
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    bail!(
        "{} opération(s) ont échoué sur {} VM.",
        failures.len(),
        refs.len()
    );
}
