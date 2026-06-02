use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "virtum",
    version,
    about = "Launch reproducible local VMs with a Docker-like workflow"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Check that the host is ready to run VMs
    Doctor,
    /// List VMs (like `docker ps`; use `-a` for stopped instances too)
    Ps {
        /// Show all instances (including shut off), not only running
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// Launch a headless VM
    Run(RunArgs),
    /// Open an SSH session to a running VM
    Shell {
        /// VM identifier returned by `virtum run`
        vm_id: String,
    },
    /// Start a previously stopped VM (same instance, no re-provisioning)
    Start {
        /// VM identifier
        vm_id: String,
    },
    /// Shut down a VM (graceful shutdown, then force after 30s)
    Stop {
        /// VM identifier
        vm_id: String,
    },
    /// Remove a VM and its local instance data
    Destroy {
        /// VM identifier
        vm_id: String,
    },
}

#[derive(clap::Args)]
pub struct RunArgs {
    /// OS image (v0.1: debian-12 only)
    #[arg(long, default_value = "debian-12")]
    pub os: String,

    /// RAM in megabytes
    #[arg(long, default_value_t = 2048)]
    pub ram: u32,

    /// Number of virtual CPUs
    #[arg(long, default_value_t = 2)]
    pub cpu: u32,

    /// Default login user (cloud-init)
    #[arg(long, default_value = "virtum")]
    pub user: String,

    /// Guest hostname; if omitted, the generated vm-id is used at run time
    #[arg(long)]
    pub hostname: Option<String>,
}
