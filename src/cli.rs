use std::path::PathBuf;

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
    /// Download and cache a cloud OS image (without starting a VM)
    Pull {
        /// OS image id (e.g. debian-12, ubuntu-24.04)
        os: String,
    },
    /// Launch a headless VM
    Run(RunArgs),
    /// Deploy all machines from a Virtumfile (default: ./Virtumfile)
    Up {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Stop or destroy all VMs of a lab (Virtumfile `name`)
    Down {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Remove VMs and instance data (like destroy), not only shut off
        #[arg(long)]
        destroy: bool,
    },
    /// Open an SSH session to a running VM
    Shell {
        /// VM id (`vm-…`) or name (`--name` from `virtum run`)
        vm: String,
    },
    /// Start one or more stopped VMs (same instances, no re-provisioning)
    Start {
        /// VM id (`vm-…`) or name (`--name` from `virtum run`)
        #[arg(value_name = "VM", num_args = 1..)]
        vms: Vec<String>,
    },
    /// Shut down one or more VMs (graceful shutdown, then force after 30s)
    Stop {
        /// VM id or name
        #[arg(value_name = "VM", num_args = 1..)]
        vms: Vec<String>,
    },
    /// Remove one or more VMs and their local instance data (orphan `vm-…` without metadata OK)
    Destroy {
        /// VM id or name
        #[arg(value_name = "VM", num_args = 1..)]
        vms: Vec<String>,
    },
}

#[derive(clap::Args)]
pub struct RunArgs {
    /// OS image (debian-12, ubuntu-24.04, …)
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

    /// Guest hostname; default: `--name` if set, else the generated vm-id
    #[arg(long)]
    pub hostname: Option<String>,

    /// Short alias for commands (`virtum shell db`). Must be unique.
    #[arg(long)]
    pub name: Option<String>,

    /// Virtual disk size for the root volume (e.g. `10G`, `20G`). Requires cloud-init grow on first boot if larger than the base image.
    #[arg(long)]
    pub disk_size: Option<String>,
}
