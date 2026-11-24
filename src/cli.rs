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
    #[command(visible_alias = "list")]
    Ps {
        /// Show all instances (including shut off), not only running
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// Download and cache a cloud OS image (without starting a VM)
    Pull {
        /// OS image id: debian-13, ubuntu-26.04 (also debian-12, ubuntu-24.04)
        os: String,
    },
    /// Launch a headless VM
    Run(RunArgs),
    /// Apply Virtumfile changes to existing lab VMs (RAM, CPU, bind, ports, disk_size, mounts, ssh_port, restart, ip)
    Apply {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Disable SSH host key checking (StrictHostKeyChecking=no); for trusted scripts only
        #[arg(long)]
        insecure_ssh: bool,
        /// Also delete lab machines missing from the Virtumfile (VM, disk, snapshots)
        #[arg(long)]
        prune: bool,
        /// With `--prune`: no interactive confirmation (scripts / CI)
        #[arg(short, long, requires = "prune")]
        yes: bool,
        /// Confirm `bind: lan` exposure (0.0.0.0) without an interactive prompt (CI / scripts)
        #[arg(long)]
        yes_lan: bool,
    },
    /// Print an Ansible inventory for the running machines of a lab (v0.8)
    Inventory {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Output format
        #[arg(long, value_enum, default_value_t = InventoryFormat::Ini)]
        format: InventoryFormat,
        /// Write to this file (atomic) instead of stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Inventory without SSH host key checking (StrictHostKeyChecking=no, like --insecure-ssh)
        #[arg(long)]
        insecure_ssh: bool,
    },
    /// Run the lab's Ansible playbook again (`ansible:` section, v0.8); args after `--` go to ansible-playbook
    Provision {
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Disable SSH host key checking (StrictHostKeyChecking=no); for trusted scripts only
        #[arg(long)]
        insecure_ssh: bool,
        /// Arguments passed to ansible-playbook (e.g. `-- --tags init --limit node-1`)
        #[arg(last = true)]
        args: Vec<String>,
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
        /// Confirm a `bind: lan` deployment without an interactive prompt (CI / scripts)
        #[arg(long)]
        yes_lan: bool,
        /// Disable SSH host key checking (StrictHostKeyChecking=no); for trusted scripts only
        #[arg(long)]
        insecure_ssh: bool,
        /// Machines created or resumed at the same time (1 = one after the other)
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..))]
        parallel: u16,
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
        /// Disable SSH host key checking (StrictHostKeyChecking=no); for trusted scripts only
        #[arg(long)]
        insecure_ssh: bool,
    },
    /// Start one or more stopped VMs (same instances, no re-provisioning)
    Start {
        /// VM id (`vm-…`) or name (`--name` from `virtum run`)
        #[arg(value_name = "VM", num_args = 1..)]
        vms: Vec<String>,
    },
    /// Shut down one or more VMs (graceful shutdown, then force after 30s; `restart: unless-stopped` then skips host boot)
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
    /// Named disk snapshots of VMs or of a whole lab (VMs are stopped while it is taken)
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Read or change Virtum settings (output language)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

/// Réglages de l'utilisateur (`~/.config/virtum/config.yml`).
#[derive(Subcommand)]
pub enum ConfigAction {
    /// Change a setting (e.g. `virtum config set language en`)
    Set {
        /// Setting name
        key: ConfigKey,
        /// New value (language: en, fr)
        value: String,
    },
    /// Show the current value of a setting and where it comes from
    Get {
        /// Setting name
        key: ConfigKey,
    },
    /// Remove a setting (back to the default)
    Unset {
        /// Setting name
        key: ConfigKey,
    },
}

/// Réglages connus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ConfigKey {
    /// Output language (en, fr); defaults to the system language
    Language,
}

/// Sans VM citée : toutes les VM du lab du Virtumfile (`-f` ou `./Virtumfile`).
#[derive(Subcommand)]
pub enum SnapshotAction {
    /// Take a named snapshot (running VMs are stopped, then restarted)
    Create {
        /// Snapshot name (letters, digits, `-`, `_`)
        name: String,
        /// VM id or name (default: whole lab)
        #[arg(value_name = "VM")]
        vms: Vec<String>,
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Leave the VMs stopped afterwards
        #[arg(long)]
        no_restart: bool,
    },
    /// Return VMs to a snapshot (running VMs are powered off, then restarted)
    Restore {
        /// Snapshot name
        name: String,
        /// VM id or name (default: whole lab)
        #[arg(value_name = "VM")]
        vms: Vec<String>,
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Leave the VMs stopped afterwards
        #[arg(long)]
        no_restart: bool,
    },
    /// List snapshots
    Ls {
        /// VM id or name (default: whole lab)
        #[arg(value_name = "VM")]
        vms: Vec<String>,
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Delete a snapshot (running VMs are stopped, then restarted)
    Rm {
        /// Snapshot name
        name: String,
        /// VM id or name (default: whole lab)
        #[arg(value_name = "VM")]
        vms: Vec<String>,
        /// Path to Virtumfile
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Leave the VMs stopped afterwards
        #[arg(long)]
        no_restart: bool,
    },
}

#[derive(clap::Args)]
pub struct RunArgs {
    /// OS image: debian-13, ubuntu-26.04 (also debian-12, ubuntu-24.04)
    #[arg(long, default_value = "debian-13")]
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

    /// Network for the VM: `default` (libvirt NAT 192.168.122.x) or `bridge` (physical LAN, macvtap)
    #[arg(long, value_enum, default_value_t = RunNetworkMode::Default)]
    pub network: RunNetworkMode,

    /// Host interface for `--network bridge` (default: the default route; see `virtum doctor`)
    #[arg(long)]
    pub bridge_interface: Option<String>,

    /// Disable SSH host key checking (StrictHostKeyChecking=no); for trusted scripts only
    #[arg(long)]
    pub insecure_ssh: bool,
}

/// Format de `virtum inventory`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum InventoryFormat {
    /// INI (format historique d'Ansible).
    #[default]
    Ini,
    /// YAML (plugin d'inventaire yaml d'Ansible).
    Yaml,
}

/// Réseau `virtum run` (hors Virtumfile).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RunNetworkMode {
    /// libvirt `default` network (NAT)
    #[default]
    Default,
    /// A NIC on the physical LAN (Ethernet recommended; see `virtum doctor`)
    Bridge,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn list_is_an_alias_of_ps() {
        let cli = Cli::try_parse_from(["virtum", "list", "-a"]).unwrap();
        assert!(matches!(cli.command, Commands::Ps { all: true }));
    }

    #[test]
    fn snapshot_create_with_vms_and_flags() {
        let cli = Cli::try_parse_from([
            "virtum",
            "snapshot",
            "create",
            "avant-tls",
            "app",
            "edge",
            "--no-restart",
        ])
        .unwrap();
        match cli.command {
            Commands::Snapshot {
                action:
                    SnapshotAction::Create {
                        name,
                        vms,
                        file,
                        no_restart,
                    },
            } => {
                assert_eq!(name, "avant-tls");
                assert_eq!(vms, vec!["app", "edge"]);
                assert!(file.is_none());
                assert!(no_restart);
            }
            _ => panic!("expected snapshot create"),
        }
    }

    #[test]
    fn snapshot_ls_defaults_to_lab() {
        let cli =
            Cli::try_parse_from(["virtum", "snapshot", "ls", "-f", "lab/Virtumfile"]).unwrap();
        match cli.command {
            Commands::Snapshot {
                action: SnapshotAction::Ls { vms, file },
            } => {
                assert!(vms.is_empty());
                assert_eq!(file, Some(PathBuf::from("lab/Virtumfile")));
            }
            _ => panic!("expected snapshot ls"),
        }
    }

    #[test]
    fn shell_insecure_ssh_after_vm_arg() {
        let cli = Cli::try_parse_from(["virtum", "shell", "test2", "--insecure-ssh"]).unwrap();
        match cli.command {
            Commands::Shell { vm, insecure_ssh } => {
                assert_eq!(vm, "test2");
                assert!(insecure_ssh);
            }
            _ => panic!("expected shell subcommand"),
        }
    }

    #[test]
    fn shell_insecure_ssh_before_vm_arg() {
        let cli = Cli::try_parse_from(["virtum", "shell", "--insecure-ssh", "test2"]).unwrap();
        match cli.command {
            Commands::Shell { vm, insecure_ssh } => {
                assert_eq!(vm, "test2");
                assert!(insecure_ssh);
            }
            _ => panic!("expected shell subcommand"),
        }
    }
}
