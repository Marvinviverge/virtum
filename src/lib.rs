pub mod ansible;
pub mod apply;
pub mod bridge_network;
pub mod cli;
pub mod cloud_init;
pub mod command;
pub mod config;
pub mod disk;
pub mod doctor;
pub mod firewall;
pub mod fsutil;
pub mod i18n;
pub mod images;
pub mod inventory;
pub mod lab;
pub mod lab_diff;
pub mod lab_network;
pub mod libvirt;
pub mod libvirt_access;
pub mod metadata;
pub mod mounts;
pub mod output;
pub mod ownership;
pub mod parallel;
pub mod plan;
pub mod port_forward;
pub mod procs;
pub mod ps;
pub mod share_access;
pub mod snapshot;
pub mod ssh;
pub mod test_env;
pub mod user_config;
pub mod virtumfile;
pub mod vm;

use anyhow::{bail, Result};
use clap::Parser;
use cli::{Cli, Commands, SnapshotAction};

pub fn run() -> Result<()> {
    init_language();
    dispatch(Cli::parse().command)
}

/// Fixe la langue de la sortie avant toute autre chose (l'aide en dépend aussi) :
/// `VIRTUM_LANG`, puis `~/.config/virtum/config.yml`, puis la locale du système.
fn init_language() {
    let loaded = user_config::load();
    let config = loaded.language.zip(loaded.path.clone());
    let resolution = i18n::resolve(|key| std::env::var(key).ok(), config);
    if let Some(warning) = &resolution.warning {
        eprintln!("warning: {warning}");
    }
    i18n::set_active(resolution);
    for warning in &loaded.warnings {
        eprintln!(
            "{}",
            tr!(fr: "avertissement : {}", en: "warning: {}", warning.message())
        );
    }
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
        Commands::Provision {
            file,
            insecure_ssh,
            args,
        } => ansible::provision(file, args, insecure_ssh)?,
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
            yes_lan,
        } => apply::apply(file, insecure_ssh, prune, yes, yes_lan)?,
        Commands::Up {
            file,
            yes_lan,
            insecure_ssh,
            parallel,
        } => lab::up(file, insecure_ssh, yes_lan, usize::from(parallel))?,
        Commands::Down {
            file,
            destroy,
            purge,
        } => lab::down(file, destroy, purge)?,
        Commands::Config { action } => user_config::command(action)?,
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
