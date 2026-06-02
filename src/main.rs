mod cli;
mod doctor;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    dispatch(Cli::parse().command)
}

fn dispatch(command: Commands) -> Result<()> {
    match command {
        Commands::Doctor => std::process::exit(doctor::run()),
        Commands::Run(args) => not_implemented(&format!(
            "virtum run --os {} --ram {} --cpu {} --user {}",
            args.os, args.ram, args.cpu, args.user
        )),
        Commands::Shell { vm_id } => not_implemented(&format!("virtum shell {vm_id}")),
        Commands::Stop { vm_id } => not_implemented(&format!("virtum stop {vm_id}")),
        Commands::Destroy { vm_id } => not_implemented(&format!("virtum destroy {vm_id}")),
    };
    Ok(())
}

fn not_implemented(command: &str) {
    eprintln!("{command}: not yet implemented");
}
