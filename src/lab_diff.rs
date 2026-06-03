//! Diff Virtumfile ↔ instances existantes et application (`virtum apply`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::VirtumPaths;
use crate::disk::{
    format_virtual_size_label, image_virtual_size_bytes, parse_disk_size, resize_cow_disk, DiskSize,
};
use crate::firewall;
use crate::images::{ensure_image, resolve_image};
use crate::lab_network;
use crate::libvirt::{
    destroy_domain, domain_exists, domain_is_active, domain_state, read_ram_vcpu_from_instance,
    redefine_domain_resources, undefine_domain,
};
use crate::metadata::{self, Instance};
use crate::port_forward::{self, PortBind, PortForward};
use crate::ssh::{ensure_ssh_keypair, run_provision_commands};
use crate::virtumfile::{self, MachineSpec, Virtumfile};
use crate::vm::{self, lab_vm_id};

const SSH_PORT: u16 = 22;
const GROW_COMMANDS: &[&str] = &[
    "growpart /dev/vda 1 || true",
    "resize2fs /dev/vda1 || xfs_growfs / || true",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineChange {
    Memory {
        from_mb: u32,
        to_mb: u32,
    },
    Cpu {
        from: u32,
        to: u32,
    },
    DiskSize {
        from_label: String,
        to_label: String,
    },
    PortBind {
        from: PortBind,
        to: PortBind,
    },
    PortForwards {
        from: Vec<PortForward>,
        to: Vec<PortForward>,
    },
    Os {
        from: String,
        to: String,
    },
}

impl MachineChange {
    pub fn summary_line(&self) -> String {
        match self {
            Self::Memory { from_mb, to_mb } => format!("ram: {from_mb} → {to_mb} MiB"),
            Self::Cpu { from, to } => format!("cpu: {from} → {to}"),
            Self::DiskSize {
                from_label,
                to_label,
            } => {
                format!("disk_size: {from_label} → {to_label}")
            }
            Self::PortBind { from, to } => {
                format!("bind: {} → {}", bind_label(*from), bind_label(*to))
            }
            Self::PortForwards { from, to } => {
                format!("ports: {} → {}", format_ports(from), format_ports(to))
            }
            Self::Os { from, to } => format!("os: {from} → {to} (non applicable)"),
        }
    }

    fn needs_guest_stop(&self) -> bool {
        matches!(
            self,
            Self::Memory { .. } | Self::Cpu { .. } | Self::DiskSize { .. }
        )
    }

    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::Os { .. })
    }
}

#[derive(Debug, Clone)]
pub struct MachineDiff {
    pub machine_name: String,
    pub vm_id: String,
    pub changes: Vec<MachineChange>,
}

#[derive(Debug, Clone)]
pub struct LabDiff {
    pub virtumfile_path: PathBuf,
    pub lab: Virtumfile,
    pub machine_diffs: Vec<MachineDiff>,
    pub creates: Vec<String>,
    pub blocking_errors: Vec<String>,
}

pub fn bind_label(bind: PortBind) -> &'static str {
    match bind {
        PortBind::Local => "local",
        PortBind::Lan => "lan",
    }
}

pub fn format_ports(forwards: &[PortForward]) -> String {
    if forwards.is_empty() {
        return "-".to_string();
    }
    forwards
        .iter()
        .map(|p| format!("{}:{}", p.host_port, p.guest_port))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Erreurs bloquantes communes à `plan` et `apply` (réservé extensions futures).
pub fn collect_lab_blocking_errors(_lab: &Virtumfile) -> Vec<String> {
    Vec::new()
}

pub fn diff_machine(
    lab: &Virtumfile,
    machine_name: &str,
    spec: &MachineSpec,
    instance: &Instance,
    instance_dir: &Path,
) -> Result<MachineDiff> {
    let vm_id = lab_vm_id(&lab.name, machine_name);
    let mut changes = Vec::new();

    if spec.os != instance.os {
        changes.push(MachineChange::Os {
            from: instance.os.clone(),
            to: spec.os.clone(),
        });
    }

    if instance_dir.join("config.xml").is_file() {
        let (ram, cpu) = read_ram_vcpu_from_instance(instance_dir)?;
        if spec.ram != ram {
            changes.push(MachineChange::Memory {
                from_mb: ram,
                to_mb: spec.ram,
            });
        }
        if spec.cpu != cpu {
            changes.push(MachineChange::Cpu {
                from: cpu,
                to: spec.cpu,
            });
        }
    }

    if let Some(ref desired_size) = spec.disk_size {
        let desired = parse_disk_size(desired_size)?;
        let disk_path = instance_dir.join("disk.qcow2");
        let current_label = if disk_path.is_file() {
            format_virtual_size_label(image_virtual_size_bytes(&disk_path)?)
        } else {
            instance
                .disk_size
                .clone()
                .unwrap_or_else(|| "?".to_string())
        };
        if current_label != *desired_size && disk_size_differs(&disk_path, &desired)? {
            changes.push(MachineChange::DiskSize {
                from_label: current_label,
                to_label: desired_size.clone(),
            });
        }
    }

    let desired_ports = virtumfile::machine_port_forwards(spec)?;
    let stored_ports = instance.port_forwards.clone().unwrap_or_default();
    // `bind` n'a d'effet que via socat : inutile de signaler un drift sans `ports:`.
    let ports_relevant = !desired_ports.is_empty() || !stored_ports.is_empty();
    if ports_relevant {
        let desired_bind = virtumfile::effective_port_bind(lab, machine_name);
        let stored_bind = instance.port_bind.unwrap_or(PortBind::Local);
        if stored_bind != desired_bind {
            changes.push(MachineChange::PortBind {
                from: stored_bind,
                to: desired_bind,
            });
        }
    }
    if stored_ports != desired_ports {
        changes.push(MachineChange::PortForwards {
            from: stored_ports,
            to: desired_ports,
        });
    }

    Ok(MachineDiff {
        machine_name: machine_name.to_string(),
        vm_id,
        changes,
    })
}

fn disk_size_differs(disk_path: &Path, desired: &DiskSize) -> Result<bool> {
    if !disk_path.is_file() {
        return Ok(true);
    }
    let current = image_virtual_size_bytes(disk_path)?;
    Ok(current != desired.bytes)
}

pub async fn build_lab_diff(virtumfile_path: PathBuf, lab: Virtumfile) -> Result<LabDiff> {
    let paths = crate::config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    let mut blocking_errors = collect_lab_blocking_errors(&lab);
    let mut creates = Vec::new();
    let mut machine_diffs = Vec::new();

    let port_plan: Vec<(String, Vec<PortForward>)> = lab
        .machines
        .iter()
        .map(|(name, spec)| virtumfile::machine_port_forwards(spec).map(|f| (name.clone(), f)))
        .collect::<Result<_>>()?;
    if let Err(e) = port_forward::validate_lab_port_uniqueness(&port_plan) {
        blocking_errors.push(e.to_string());
    }

    for (machine_name, spec) in &lab.machines {
        let vm_id = lab_vm_id(&lab.name, machine_name);
        match metadata::find_by_lab_machine(&metadata, &lab.name, machine_name) {
            None => creates.push(machine_name.clone()),
            Some(inst) => {
                if inst.id != vm_id {
                    blocking_errors.push(format!(
                        "Machine '{machine_name}' : metadata {} ≠ domaine attendu {vm_id}.",
                        inst.id
                    ));
                    continue;
                }
                let instance_dir = paths.instances.join(&vm_id);
                let diff = diff_machine(&lab, machine_name, spec, inst, &instance_dir)?;
                if !diff.changes.is_empty() {
                    machine_diffs.push(diff);
                }
            }
        }
    }
    machine_diffs.sort_by(|a, b| a.machine_name.cmp(&b.machine_name));

    Ok(LabDiff {
        virtumfile_path,
        lab,
        machine_diffs,
        creates,
        blocking_errors,
    })
}

pub async fn apply_lab(file: Option<PathBuf>) -> Result<()> {
    let virtumfile_path = virtumfile::resolve_virtumfile_path(file)?;
    let lab = virtumfile::load(&virtumfile_path)?;
    let diff = build_lab_diff(virtumfile_path.clone(), lab.clone()).await?;

    if !diff.blocking_errors.is_empty() {
        for e in &diff.blocking_errors {
            eprintln!("✗ {e}");
        }
        bail!(
            "Impossible d'appliquer : {} erreur(s) bloquante(s).",
            diff.blocking_errors.len()
        );
    }

    if diff.machine_diffs.is_empty() {
        println!("Aucune modification à appliquer sur le lab '{}'.", lab.name);
        if !diff.creates.is_empty() {
            println!(
                "Machines absentes : {}. Lancez : virtum up -f {}",
                diff.creates.join(", "),
                virtumfile_path.display()
            );
        }
        return Ok(());
    }

    let paths = crate::config::ensure_layout().await?;
    let lab_net = lab_network::lab_network_name(&lab.name).ok();

    println!(
        "Application de {} modification(s) sur le lab '{}'…",
        diff.machine_diffs.len(),
        lab.name
    );

    let mut needs_open_hint = false;
    for md in &diff.machine_diffs {
        if md
            .changes
            .iter()
            .any(|c| matches!(c, MachineChange::Os { .. }))
        {
            bail!(
                "La machine '{}' demande un changement d'OS (non supporté par apply). \
                 Utilisez `virtum down -f {} --destroy` puis `virtum up`.",
                md.machine_name,
                virtumfile_path.display()
            );
        }
        for c in &md.changes {
            if c.is_blocking() {
                continue;
            }
            println!("  {} : {}", md.machine_name, c.summary_line());
        }

        let spec = &lab.machines[&md.machine_name];
        apply_machine(&paths, &lab, &lab.name, lab_net.as_deref(), spec, md).await?;

        if md.changes.iter().any(|c| {
            matches!(
                c,
                MachineChange::PortBind {
                    to: PortBind::Lan,
                    ..
                } | MachineChange::PortForwards { .. }
            )
        }) && virtumfile::effective_port_bind(&lab, &md.machine_name) == PortBind::Lan
        {
            needs_open_hint = true;
        }
    }

    if needs_open_hint && !firewall::has_firewall_state(&paths, &lab.name)? {
        println!(
            "Pare-feu LAN : lancez `virtum open -f {}` si ce n'est pas déjà fait.",
            virtumfile_path.display()
        );
    }

    println!("Apply terminé pour le lab '{}'.", lab.name);
    Ok(())
}

async fn apply_machine(
    paths: &VirtumPaths,
    lab: &Virtumfile,
    lab_name: &str,
    lab_net: Option<&str>,
    spec: &MachineSpec,
    md: &MachineDiff,
) -> Result<()> {
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    let instance = metadata::find_by_lab_machine(&metadata, lab_name, &md.machine_name)
        .context("Instance introuvable pendant apply")?;
    let instance_dir = paths.instances.join(&md.vm_id);
    let vm_id = &md.vm_id;

    let needs_stop = md.changes.iter().any(|c| c.needs_guest_stop());
    let was_running = domain_exists(vm_id)
        && domain_state(vm_id)
            .map(|s| domain_is_active(&s))
            .unwrap_or(false);

    port_forward::stop_instance_proxies(&instance_dir);

    if needs_stop && was_running {
        eprintln!("  Arrêt de '{vm_id}' pour appliquer les changements…");
        vm::stop_vm_async(instance.name.as_deref().unwrap_or(vm_id)).await?;
    } else if needs_stop && domain_exists(vm_id) {
        destroy_domain(vm_id)?;
        undefine_domain(vm_id)?;
    }

    let mut grow_guest = false;
    let mut new_disk_label: Option<String> = None;

    for change in &md.changes {
        match change {
            MachineChange::Memory { .. } | MachineChange::Cpu { .. } => {}
            MachineChange::DiskSize { to_label, .. } => {
                let size = parse_disk_size(to_label)?;
                let disk_path = instance_dir.join("disk.qcow2");
                let image = resolve_image(&spec.os, &paths.images)?;
                let _ = ensure_image(&image).await?;
                if resize_cow_disk(&disk_path, &size)? {
                    grow_guest = true;
                }
                new_disk_label = Some(to_label.clone());
            }
            MachineChange::PortBind { .. } | MachineChange::PortForwards { .. } => {}
            MachineChange::Os { .. } => unreachable!(),
        }
    }

    let needs_redefine = md
        .changes
        .iter()
        .any(|c| matches!(c, MachineChange::Memory { .. } | MachineChange::Cpu { .. }));
    if needs_redefine {
        let net_cfg = virtumfile::resolve_machine_network(lab, &md.machine_name, lab_net)?;
        redefine_domain_resources(
            &instance_dir,
            vm_id,
            spec.ram,
            spec.cpu,
            net_cfg.libvirt_network.as_deref(),
            net_cfg.bridge_host_interface.as_deref(),
        )?;
        eprintln!(
            "  Domaine '{vm_id}' redéfini (RAM {} MiB, {} vCPU).",
            spec.ram, spec.cpu
        );
    }

    let desired_ports = virtumfile::machine_port_forwards(spec)?;
    let desired_bind = virtumfile::effective_port_bind(lab, &md.machine_name);
    if !desired_ports.is_empty() {
        port_forward::ensure_ports_available(&desired_ports, desired_bind)?;
    }

    let mut updated = instance.clone();
    updated.port_forwards = if desired_ports.is_empty() {
        None
    } else {
        Some(desired_ports.clone())
    };
    updated.port_bind = if desired_ports.is_empty() {
        None
    } else {
        Some(desired_bind)
    };
    if let Some(label) = new_disk_label {
        updated.disk_size = Some(label);
    } else if spec.disk_size.is_some() {
        updated.disk_size = spec.disk_size.clone();
    }
    metadata::upsert_instance(&paths.metadata, updated).await?;

    if was_running && needs_stop {
        let reference = instance.name.clone().unwrap_or_else(|| vm_id.clone());
        eprintln!("  Redémarrage de '{reference}'…");
        vm::start_vm_async(&reference).await?;
    }

    if grow_guest {
        let meta = metadata::load_metadata(&paths.metadata).await?;
        let inst = metadata::find_by_lab_machine(&meta, lab_name, &md.machine_name)
            .context("metadata après apply")?;
        if let Some(ref ip) = inst.ip {
            if domain_exists(vm_id)
                && domain_state(vm_id)
                    .map(|s| domain_is_active(&s))
                    .unwrap_or(false)
            {
                let keys = ensure_ssh_keypair(&paths.keys)?;
                eprintln!("  Extension de la partition racine sur '{ip}'…");
                let cmds: Vec<String> = GROW_COMMANDS.iter().map(|s| s.to_string()).collect();
                run_provision_commands(
                    &keys.private_key,
                    &inst.user,
                    ip,
                    SSH_PORT,
                    &cmds,
                    std::time::Duration::from_secs(120),
                )?;
            }
        } else {
            eprintln!(
                "  Disque qcow2 agrandi. Démarrez la VM (`virtum start` / `virtum up`) puis relancez apply si la partition invitée n'a pas grandi."
            );
        }
    }

    if !desired_ports.is_empty() {
        let meta = metadata::load_metadata(&paths.metadata).await?;
        let inst = metadata::find_by_lab_machine(&meta, lab_name, &md.machine_name)
            .context("metadata pour socat")?;
        if let Some(ref ip) = inst.ip {
            if domain_exists(vm_id)
                && domain_state(vm_id)
                    .map(|s| domain_is_active(&s))
                    .unwrap_or(false)
            {
                port_forward::start_instance_proxies(
                    &instance_dir,
                    ip,
                    &desired_ports,
                    desired_bind,
                )?;
                port_forward::print_forward_lines(&desired_ports, desired_bind);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_forward::PortForward;
    use crate::virtumfile::{MachineSpec, Virtumfile};
    use std::collections::BTreeMap;

    #[test]
    fn change_summary_memory() {
        let c = MachineChange::Memory {
            from_mb: 2048,
            to_mb: 4096,
        };
        assert!(c.summary_line().contains("4096"));
    }

    #[test]
    fn bind_drift_ignored_when_no_ports() {
        let lab = Virtumfile {
            name: "lab".into(),
            network: None,
            bridge_interface: None,
            bind: Some(PortBind::Lan),
            machines: BTreeMap::from([(
                "db".into(),
                MachineSpec {
                    os: "debian-12".into(),
                    ram: 1024,
                    cpu: 1,
                    disk_size: None,
                    network: None,
                    bridge_interface: None,
                    ports: vec![],
                    bind: None,
                    provision: vec![],
                },
            )]),
        };
        let instance = Instance {
            id: "vm-test".into(),
            name: Some("db".into()),
            os: "debian-12".into(),
            status: crate::metadata::InstanceStatus::Stopped,
            ip: None,
            ssh_port: None,
            user: "virtum".into(),
            hostname: "db".into(),
            disk_size: None,
            lab_name: Some("lab".into()),
            machine_name: Some("db".into()),
            bridge_host_interface: None,
            lan_ip: None,
            port_forwards: None,
            port_bind: Some(PortBind::Local),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let dir = std::env::temp_dir().join(format!("virtum-diff-bind-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let diff = diff_machine(&lab, "db", &lab.machines["db"], &instance, &dir).unwrap();
        assert!(
            !diff
                .changes
                .iter()
                .any(|c| matches!(c, MachineChange::PortBind { .. })),
            "bind ne doit pas être signalé sans ports: {:?}",
            diff.changes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_drift_detected_when_ports_present() {
        let lab = Virtumfile {
            name: "lab".into(),
            network: None,
            bridge_interface: None,
            bind: Some(PortBind::Lan),
            machines: BTreeMap::from([(
                "web".into(),
                MachineSpec {
                    os: "debian-12".into(),
                    ram: 2048,
                    cpu: 2,
                    disk_size: None,
                    network: None,
                    bridge_interface: None,
                    ports: vec!["18080:80".into()],
                    bind: None,
                    provision: vec![],
                },
            )]),
        };
        let instance = Instance {
            id: "vm-web".into(),
            name: Some("web".into()),
            os: "debian-12".into(),
            status: crate::metadata::InstanceStatus::Stopped,
            ip: Some("192.168.68.2".into()),
            ssh_port: None,
            user: "virtum".into(),
            hostname: "web".into(),
            disk_size: None,
            lab_name: Some("lab".into()),
            machine_name: Some("web".into()),
            bridge_host_interface: None,
            lan_ip: None,
            port_forwards: Some(vec![PortForward {
                host_port: 18080,
                guest_port: 80,
            }]),
            port_bind: Some(PortBind::Local),
            created_at: "2020-01-01T00:00:00Z".into(),
        };
        let dir = std::env::temp_dir().join(format!("virtum-diff-bind2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let diff = diff_machine(&lab, "web", &lab.machines["web"], &instance, &dir).unwrap();
        assert!(
            diff.changes
                .iter()
                .any(|c| matches!(c, MachineChange::PortBind { .. })),
            "bind doit être signalé avec ports: {:?}",
            diff.changes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
