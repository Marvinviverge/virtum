pub mod cli;
pub mod cloud_init;
pub mod command;
pub mod config;
pub mod disk;
pub mod doctor;
pub mod images;
pub mod libvirt;
pub mod libvirt_access;
pub mod metadata;
pub mod ps;
pub mod ssh;
pub mod test_env;
pub mod vm;

use anyhow::Result;
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
        Commands::Shell { vm_id } => ssh::shell_to_instance(&vm_id)?,
        Commands::Start { vm_id } => vm::start_vm(&vm_id)?,
        Commands::Stop { vm_id } => vm::stop_vm(&vm_id)?,
        Commands::Destroy { vm_id } => vm::destroy_vm(&vm_id)?,
    };
    Ok(())
}
