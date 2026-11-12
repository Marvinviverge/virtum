pub mod apply;
pub mod bridge_network;
pub mod cli;
pub mod cloud_init;
pub mod command;
pub mod config;
pub mod disk;
pub mod doctor;
pub mod firewall;
pub mod images;
pub mod inventory;
pub mod lab;
pub mod lab_diff;
pub mod lab_network;
pub mod libvirt;
pub mod libvirt_access;
pub mod metadata;
pub mod mounts;
pub mod plan;
pub mod port_forward;
pub mod ps;
pub mod snapshot;
pub mod ssh;
pub mod test_env;
pub mod virtumfile;
pub mod vm;

use anyhow::{bail, Result};
use clap::Parser;
use cli::{Cli, Commands, SnapshotAction};

pub fn run() -> Result<()> {
    dispatch(Cli::parse().command)
}

fn dispatch(command: Commands) -> Result<()> {
    match command {
        Commands::Doctor => std::process::exit(doctor::run()),
        Commands::Ps { all } => ps::ps(all)?,
        Commands::Pull { os } => images::pull(&os)?,
        Commands::Run(args) => vm::run_vm(args)?,
        Commands::Shell { vm, insecure_ssh } => ssh::shell_to_instance(&vm, insecure_ssh)?,
        Commands::Start { vms } => run_for_each_vm(&vms, vm::start_vm)?,
        Commands::Stop { vms } => run_for_each_vm(&vms, vm::stop_vm)?,
        Commands::Destroy { vms, purge } => run_for_each_vm_destroy(&vms, purge)?,
        Commands::Open { file } => firewall::open(file)?,
        Commands::Close { file } => firewall::close(file)?,
        Commands::Plan { file } => plan::plan(file)?,
        Commands::Inventory {
            file,
            format,
            output,
            insecure_ssh,
        } => inventory::inventory(file, format, output, insecure_ssh)?,
        Commands::Apply {
            file,
            insecure_ssh,
            prune,
            yes,
        } => apply::apply(file, insecure_ssh, prune, yes)?,
        Commands::Up {
            file,
            yes_lan,
            insecure_ssh,
        } => lab::up(file, insecure_ssh, yes_lan)?,
        Commands::Down {
            file,
            destroy,
            purge,
        } => lab::down(file, destroy, purge)?,
        Commands::Snapshot { action } => match action {
            SnapshotAction::Create {
                name,
                vms,
                file,
                no_restart,
            } => snapshot::create(&name, &vms, file, no_restart)?,
            SnapshotAction::Restore {
                name,
                vms,
                file,
                no_restart,
            } => snapshot::restore(&name, &vms, file, no_restart)?,
            SnapshotAction::Ls { vms, file } => snapshot::list(&vms, file)?,
            SnapshotAction::Rm {
                name,
                vms,
                file,
                no_restart,
            } => snapshot::remove(&name, &vms, file, no_restart)?,
        },
    };
    Ok(())
}

/// Exécute une opération sur chaque référence VM ; continue après échec, puis résume les erreurs.
fn run_for_each_vm_destroy(refs: &[String], purge: bool) -> Result<()> {
    let mut failures = Vec::new();
    for reference in refs {
        let reference = reference.trim();
        if reference.is_empty() {
            continue;
        }
        if let Err(err) = vm::destroy_vm(reference, purge) {
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
