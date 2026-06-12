use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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
    /// Apply Virtumfile changes to existing lab VMs (RAM, bind, ports, disk_size)
    Apply {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Preview lab deploy (create/resume, updates, warnings) without changes
    Plan {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Deploy all machines from a Virtumfile (default: ./Virtumfile)
    Up {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Open host firewall (ufw) for `bind: lan` ports in a Virtumfile
    Open {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Remove Virtum-added ufw rules for a lab (`virtum open`)
    Close {
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
        /// With `--destroy`: also delete host directories declared in `mounts:` (under the Virtumfile)
        #[arg(long)]
        purge: bool,
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
        /// Also delete host `mounts:` directories for the destroyed VM(s)
        #[arg(long)]
        purge: bool,
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

    /// Réseau pour la VM isolée : `default` (libvirt NAT 192.168.122.x) ou `bridge` (LAN physique, macvtap).
    #[arg(long, value_enum, default_value_t = RunNetworkMode::Default)]
    pub network: RunNetworkMode,

    /// Interface hôte pour `--network bridge` (sinon route par défaut / `virtum doctor`).
    #[arg(long)]
    pub bridge_interface: Option<String>,
}

/// Réseau `virtum run` (hors Virtumfile).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RunNetworkMode {
    /// Réseau libvirt `default` (comportement v0.1/v0.2).
    #[default]
    Default,
    /// Une NIC sur le LAN du bureau (nécessite Ethernet recommandé ; voir `virtum doctor`).
    Bridge,
}
