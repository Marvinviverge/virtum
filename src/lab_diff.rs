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
    destroy_domain, domain_exists, domain_is_active, domain_state, read_ram_vcpu_from_domain,
    read_ram_vcpu_from_instance, redefine_domain_resources, undefine_domain,
};
use crate::metadata::{self, Instance};
use crate::mounts::{self, ResolvedMount};
use crate::port_forward::{self, PortBind, PortForward};
use crate::ssh::{
    ensure_ssh_keypair, reachable_ssh_port, run_provision_commands, run_ssh_mount_steps,
    SshSessionOpts,
};
use crate::virtumfile::{self, MachineSpec, MountSpec, RestartPolicy, Virtumfile};
use crate::vm::{self, lab_vm_id};

const GROW_COMMANDS: &[&str] = &[
    "growpart /dev/vda 1 || true",
    "resize2fs /dev/vda1 || xfs_growfs / || true",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineChange {
    /// `external` : la définition libvirt diffère de `config.xml` (modifiée hors de Virtum).
    Memory {
        from_mb: u32,
        to_mb: u32,
        external: bool,
    },
    Cpu {
        from: u32,
        to: u32,
        external: bool,
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
    /// Nouveaux volumes virtiofs à ajouter (virtum apply).
    MountsAdd {
        volumes: Vec<MountSpec>,
    },
    /// Port sshd déclaré (metadata seulement, aucun redémarrage).
    SshPort {
        from: u16,
        to: u16,
    },
    /// Démarrage au boot de l'hôte (autostart libvirt, aucun redémarrage).
    Restart {
        from: Option<RestartPolicy>,
        to: Option<RestartPolicy>,
    },
    /// Adresse `ip:` déclarée (réservation DHCP ; redémarrage si une adresse est posée).
    StaticIp {
        from: Option<String>,
        to: Option<String>,
    },
}

impl MachineChange {
    pub fn summary_line(&self) -> String {
        match self {
            Self::Memory {
                from_mb,
                to_mb,
                external,
            } => format!("ram: {from_mb} → {to_mb} MiB{}", external_note(*external)),
            Self::Cpu { from, to, external } => {
                format!("cpu: {from} → {to}{}", external_note(*external))
            }
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
            Self::MountsAdd { volumes } => {
                format!(
                    "mounts: +{} ({})",
                    volumes.len(),
                    mounts::format_mount_list(volumes)
                )
            }
            Self::SshPort { from, to } => format!("ssh_port: {from} → {to}"),
            Self::Restart { from, to } => {
                format!("restart: {} → {}", restart_label(*from), restart_label(*to))
            }
            Self::StaticIp { from, to } => {
                let label = |ip: &Option<String>| ip.clone().unwrap_or_else(|| "(dhcp)".into());
                let note = if to.is_some() {
                    " (redémarrage de la VM)"
                } else {
                    " (adresse actuelle conservée jusqu'à la fin du bail)"
                };
                format!("ip: {} → {}{note}", label(from), label(to))
            }
        }
    }

    /// False si `virtum up` applique lui-même le changement (sans exiger `apply`).
    pub fn needs_apply(&self) -> bool {
        !matches!(self, Self::SshPort { .. } | Self::Restart { .. })
    }

    fn needs_guest_stop(&self) -> bool {
        matches!(
            self,
            Self::Memory { .. } | Self::Cpu { .. } | Self::DiskSize { .. } | Self::MountsAdd { .. }
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

fn external_note(external: bool) -> &'static str {
    if external {
        " (modifié hors de Virtum)"
    } else {
        ""
    }
}

/// Écart entre la valeur voulue et l'existant : `(valeur actuelle, modifiée hors de Virtum)`.
///
/// `live` : définition libvirt persistante (prioritaire) ; `local` : `config.xml` de Virtum.
/// Si libvirt est déjà aligné mais `config.xml` ne l'est pas, l'écart est signalé quand même :
/// `config.xml` sert à redéfinir le domaine (`refresh_domain_xml`) et doit être réaligné.
fn resource_drift(desired: u32, live: Option<u32>, local: Option<u32>) -> Option<(u32, bool)> {
    match (live, local) {
        (Some(l), _) if l != desired => Some((l, local.is_some_and(|c| c != l))),
        (Some(_), Some(c)) if c != desired => Some((c, true)),
        (None, Some(c)) if c != desired => Some((c, false)),
        _ => None,
    }
}

/// RAM/vCPU de la définition libvirt, `None` si le domaine est absent ou illisible.
fn live_ram_vcpu(vm_id: &str) -> Option<(u32, u32)> {
    if !domain_exists(vm_id) {
        return None;
    }
    match read_ram_vcpu_from_domain(vm_id) {
        Ok(values) => Some(values),
        Err(err) => {
            eprintln!(
                "Avertissement: RAM/vCPU illisibles dans libvirt pour {vm_id} ({err:#}) ; \
                 comparaison avec config.xml."
            );
            None
        }
    }
}

fn restart_label(restart: Option<RestartPolicy>) -> &'static str {
    restart.map_or("(non géré)", RestartPolicy::label)
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

/// Erreurs bloquantes communes à `plan` et `apply`.
pub fn collect_lab_blocking_errors(
    lab: &Virtumfile,
    virtumfile_path: &Path,
    metadata: &metadata::Metadata,
) -> Vec<String> {
    let dir = virtumfile::virtumfile_dir(virtumfile_path);
    let mut errors = Vec::new();
    for machine_name in lab.machines.keys() {
        if let Err(e) = mounts::resolve_machine_mounts(lab, machine_name, &dir) {
            errors.push(format!("Machine '{machine_name}' : {e:#}"));
        }
        if let Some(inst) = metadata::find_by_lab_machine(metadata, &lab.name, machine_name) {
            let stored = inst.mounts.clone().unwrap_or_default();
            let desired = virtumfile::effective_mount_specs(lab, machine_name);
            for msg in mounts::mounts_changed_blocking(&stored, &desired) {
                errors.push(format!("Machine '{machine_name}' : {msg}"));
            }
        }
    }
    errors
}

/// Avertissements mounts retirés du Virtumfile mais encore actifs en metadata.
pub fn collect_mount_warnings(lab: &Virtumfile, metadata: &metadata::Metadata) -> Vec<String> {
    let mut warnings = Vec::new();
    for machine_name in lab.machines.keys() {
        let Some(inst) = metadata::find_by_lab_machine(metadata, &lab.name, machine_name) else {
            continue;
        };
        let stored = inst.mounts.clone().unwrap_or_default();
        let desired = virtumfile::effective_mount_specs(lab, machine_name);
        for name in mounts::mounts_removed_from_file(&stored, &desired) {
            warnings.push(format!(
                "Machine '{machine_name}' : volume mount '{name}' retiré du Virtumfile mais toujours actif sur la VM (destroy pour retirer)."
            ));
        }
    }
    warnings
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

    let local = if instance_dir.join("config.xml").is_file() {
        Some(read_ram_vcpu_from_instance(instance_dir)?)
    } else {
        None
    };
    let live = live_ram_vcpu(&instance.id);
    if let Some((from_mb, external)) =
        resource_drift(spec.ram, live.map(|v| v.0), local.map(|v| v.0))
    {
        changes.push(MachineChange::Memory {
            from_mb,
            to_mb: spec.ram,
            external,
        });
    }
    if let Some((from, external)) = resource_drift(spec.cpu, live.map(|v| v.1), local.map(|v| v.1))
    {
        changes.push(MachineChange::Cpu {
            from,
            to: spec.cpu,
            external,
        });
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

    let desired_mounts = virtumfile::effective_mount_specs(lab, machine_name);
    let stored_mounts = instance.mounts.clone().unwrap_or_default();
    let additions: Vec<MountSpec> = mounts::mounts_to_add(&desired_mounts, &stored_mounts)
        .into_iter()
        .cloned()
        .collect();
    if !additions.is_empty() {
        changes.push(MachineChange::MountsAdd { volumes: additions });
    }

    let desired_ip = virtumfile::machine_static_ip(spec).map(|ip| ip.to_string());
    if instance.static_ip != desired_ip {
        changes.push(MachineChange::StaticIp {
            from: instance.static_ip.clone(),
            to: desired_ip,
        });
    }

    let desired_restart = virtumfile::effective_restart(lab, machine_name);
    if instance.restart != desired_restart {
        changes.push(MachineChange::Restart {
            from: instance.restart,
            to: desired_restart,
        });
    }

    let stored_ssh_port = instance.ssh_port.unwrap_or(virtumfile::DEFAULT_SSH_PORT);
    let desired_ssh_port = virtumfile::effective_ssh_port(lab, machine_name);
    if stored_ssh_port != desired_ssh_port {
        changes.push(MachineChange::SshPort {
            from: stored_ssh_port,
            to: desired_ssh_port,
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
    let mut blocking_errors = collect_lab_blocking_errors(&lab, &virtumfile_path, &metadata);
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
    if let Ok(net) = lab_network::lab_network_name(&lab.name) {
        let owners = lab_network::lab_instance_macs(
            &paths.instances,
            &metadata::instances_for_lab(&metadata, &lab.name),
        );
        blocking_errors.extend(crate::lab::declared_ip_conflicts(&lab, &net, &owners));
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

pub async fn apply_lab(file: Option<PathBuf>, insecure_ssh: bool) -> Result<()> {
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
        apply_machine(
            &paths,
            &lab,
            &lab.name,
            lab_net.as_deref(),
            spec,
            md,
            &virtumfile_path,
            insecure_ssh,
        )
        .await?;

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

#[allow(clippy::too_many_arguments)]
async fn apply_machine(
    paths: &VirtumPaths,
    lab: &Virtumfile,
    lab_name: &str,
    lab_net: Option<&str>,
    spec: &MachineSpec,
    md: &MachineDiff,
    virtumfile_path: &Path,
    insecure_ssh: bool,
) -> Result<()> {
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    let instance = metadata::find_by_lab_machine(&metadata, lab_name, &md.machine_name)
        .context("Instance introuvable pendant apply")?;
    let instance_dir = paths.instances.join(&md.vm_id);
    let vm_id = &md.vm_id;
    let ssh_opts = SshSessionOpts::new(&instance_dir, insecure_ssh);

    let needs_stop = md.changes.iter().any(|c| c.needs_guest_stop());
    // Nouvelle adresse : la VM doit redemander un bail (arrêt / démarrage, sans undefine).
    let ip_renew = md
        .changes
        .iter()
        .any(|c| matches!(c, MachineChange::StaticIp { to: Some(_), .. }));
    let was_running = domain_exists(vm_id)
        && domain_state(vm_id)
            .map(|s| domain_is_active(&s))
            .unwrap_or(false);

    port_forward::stop_instance_proxies(&instance_dir);

    if (needs_stop || ip_renew) && was_running {
        eprintln!("  Arrêt de '{vm_id}' pour appliquer les changements…");
        vm::stop_vm_async(instance.name.as_deref().unwrap_or(vm_id)).await?;
    } else if needs_stop && domain_exists(vm_id) {
        destroy_domain(vm_id)?;
        undefine_domain(vm_id)?;
    }

    let mut grow_guest = false;
    let mut new_disk_label: Option<String> = None;
    let mut mounts_added_ssh: Vec<ResolvedMount> = Vec::new();

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
            MachineChange::MountsAdd { volumes } => {
                mounts_added_ssh =
                    apply_mounts_add(paths, lab, lab_net, spec, md, volumes, virtumfile_path)
                        .await?;
            }
            MachineChange::SshPort { .. } | MachineChange::Restart { .. } => {}
            MachineChange::StaticIp { to, .. } => {
                apply_static_ip(
                    lab_net,
                    &instance_dir,
                    vm_id,
                    &instance.hostname,
                    to.as_deref(),
                )?;
            }
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
    updated.ssh_port = vm::stored_ssh_port(virtumfile::effective_ssh_port(lab, &md.machine_name));
    updated.static_ip = virtumfile::machine_static_ip(spec).map(|ip| ip.to_string());
    if ip_renew {
        // Indice pour `start` : la VM revient sur l'adresse déclarée.
        updated.ip = updated.static_ip.clone();
    }
    let previous_restart = updated.restart;
    updated.restart = virtumfile::effective_restart(lab, &md.machine_name);
    let desired_mounts = virtumfile::effective_mount_specs(lab, &md.machine_name);
    updated.mounts = if desired_mounts.is_empty() {
        None
    } else {
        Some(desired_mounts)
    };
    metadata::upsert_instance(&paths.metadata, updated).await?;

    if was_running && (needs_stop || ip_renew) {
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
                    reachable_ssh_port(ip, inst.ssh_port.unwrap_or(virtumfile::DEFAULT_SSH_PORT)),
                    &cmds,
                    std::time::Duration::from_secs(120),
                    &ssh_opts,
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

    if !mounts_added_ssh.is_empty() {
        let meta = metadata::load_metadata(&paths.metadata).await?;
        let inst = metadata::find_by_lab_machine(&meta, lab_name, &md.machine_name)
            .context("metadata pour mounts SSH")?;
        if let Some(ref ip) = inst.ip {
            if domain_exists(vm_id)
                && domain_state(vm_id)
                    .map(|s| domain_is_active(&s))
                    .unwrap_or(false)
            {
                let keys = ensure_ssh_keypair(&paths.keys)?;
                eprintln!(
                    "  Montage virtiofs dans la VM ({} volume(s))…",
                    mounts_added_ssh.len()
                );
                run_ssh_mount_steps(
                    &keys.private_key,
                    &inst.user,
                    ip,
                    reachable_ssh_port(ip, inst.ssh_port.unwrap_or(virtumfile::DEFAULT_SSH_PORT)),
                    &mounts_added_ssh,
                    &ssh_opts,
                )?;
            }
        }
    }

    // En dernier : un undefine/define plus haut efface l'autostart libvirt.
    if domain_exists(vm_id) {
        let meta = metadata::load_metadata(&paths.metadata).await?;
        if let Some(inst) = metadata::find_by_lab_machine(&meta, lab_name, &md.machine_name) {
            let running = domain_state(vm_id)
                .map(|s| domain_is_active(&s))
                .unwrap_or(false);
            if inst.restart.is_none() && previous_restart.is_some() {
                crate::libvirt::set_domain_autostart(vm_id, false)?;
            }
            vm::sync_autostart(inst, running)?;
        }
    }

    Ok(())
}

/// Pose, déplace ou retire la réservation DHCP de la carte lab d'une machine.
fn apply_static_ip(
    lab_net: Option<&str>,
    instance_dir: &Path,
    vm_id: &str,
    hostname: &str,
    ip: Option<&str>,
) -> Result<()> {
    let net = lab_net.context("`ip:` nécessite le réseau du lab (virtum-lab-*)")?;
    let mac = crate::libvirt::resolve_nic_mac(instance_dir, vm_id)
        .with_context(|| format!("MAC de la carte lab introuvable pour {vm_id}"))?;
    match ip {
        Some(ip) => {
            lab_network::ensure_dhcp_reservation(net, &mac, hostname, ip)?;
            eprintln!("  Réservation DHCP : {ip} pour {mac}.");
        }
        None => {
            lab_network::remove_dhcp_reservations(net, &mac)?;
            eprintln!(
                "  Réservation DHCP retirée : la VM garde son adresse actuelle jusqu'à la fin du bail."
            );
        }
    }
    Ok(())
}

async fn apply_mounts_add(
    paths: &VirtumPaths,
    lab: &Virtumfile,
    lab_net: Option<&str>,
    spec: &MachineSpec,
    md: &MachineDiff,
    additions: &[MountSpec],
    virtumfile_path: &Path,
) -> Result<Vec<ResolvedMount>> {
    let vf_dir = virtumfile::virtumfile_dir(virtumfile_path);
    let instance_dir = paths.instances.join(&md.vm_id);
    let vm_id = &md.vm_id;

    let mut state = mounts::load_mounts_state(&instance_dir)?;
    let mut newly_added = Vec::new();
    for add in additions {
        if state.iter().any(|m| m.name == add.name) {
            continue;
        }
        let mut resolved = mounts::resolve_mount_spec(add, &vf_dir)?;
        resolved.socket_path = mounts::socket_path_for(&instance_dir, &resolved.name);
        newly_added.push(resolved.clone());
        state.push(resolved);
    }
    if newly_added.is_empty() {
        return Ok(Vec::new());
    }
    mounts::ensure_virtiofs_servers(&instance_dir, &mut state)?;

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
        "  Domaine '{vm_id}' redéfini ({} nouveau(x) mount(s) virtiofs).",
        newly_added.len()
    );
    Ok(newly_added)
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
            external: false,
        };
        assert_eq!(c.summary_line(), "ram: 2048 → 4096 MiB");
        let c = MachineChange::Cpu {
            from: 4,
            to: 2,
            external: true,
        };
        assert_eq!(c.summary_line(), "cpu: 4 → 2 (modifié hors de Virtum)");
    }

    #[test]
    fn resource_drift_prefers_libvirt_and_flags_external_changes() {
        // Tout est aligné.
        assert_eq!(resource_drift(2048, Some(2048), Some(2048)), None);
        // Changement du Virtumfile, rien hors de Virtum.
        assert_eq!(
            resource_drift(4096, Some(2048), Some(2048)),
            Some((2048, false))
        );
        // `virsh setmem --config` hors de Virtum : libvirt fait foi.
        assert_eq!(
            resource_drift(2048, Some(1024), Some(2048)),
            Some((1024, true))
        );
        // libvirt déjà aligné à la main, config.xml en retard : à réaligner.
        assert_eq!(
            resource_drift(2048, Some(2048), Some(1024)),
            Some((1024, true))
        );
        // Domaine absent : repli sur config.xml.
        assert_eq!(resource_drift(2048, None, Some(1024)), Some((1024, false)));
        assert_eq!(resource_drift(2048, None, Some(2048)), None);
        // Ni domaine ni config.xml : rien à comparer.
        assert_eq!(resource_drift(2048, None, None), None);
    }

    #[test]
    fn static_ip_change_is_detected() {
        let octet = lab_network::lab_subnet_third_octet("lab");
        let lab = virtumfile::load_from_str(&format!(
            "name: lab\nmachines:\n  app:\n    os: ubuntu-24.04\n    ip: 192.168.{octet}.10\n"
        ))
        .unwrap();
        let instance: Instance = serde_json::from_str(
            r#"{"id":"vm-test-ip","name":"app","os":"ubuntu-24.04","status":"stopped",
                "user":"virtum","hostname":"lab-app","created_at":"2026-11-12T09:00:00Z"}"#,
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("virtum-diff-ip-{}", std::process::id()));
        let diff = diff_machine(&lab, "app", &lab.machines["app"], &instance, &dir).unwrap();
        let wanted = format!("192.168.{octet}.10");
        assert_eq!(
            diff.changes,
            vec![MachineChange::StaticIp {
                from: None,
                to: Some(wanted.clone()),
            }]
        );
        assert!(diff.changes[0].needs_apply());
        assert_eq!(
            diff.changes[0].summary_line(),
            format!("ip: (dhcp) → {wanted} (redémarrage de la VM)")
        );

        let mut aligned = instance.clone();
        aligned.static_ip = Some(wanted.clone());
        let diff = diff_machine(&lab, "app", &lab.machines["app"], &aligned, &dir).unwrap();
        assert!(diff.changes.is_empty());

        let removed = MachineChange::StaticIp {
            from: Some(wanted),
            to: None,
        };
        assert!(removed.summary_line().contains("→ (dhcp)"));
    }

    #[test]
    fn ssh_port_drift_is_reported_but_left_to_up() {
        let lab = virtumfile::load_from_str(
            "name: lab\nssh_port: 2222\nmachines:\n  app:\n    os: ubuntu-24.04\n",
        )
        .unwrap();
        let instance: Instance = serde_json::from_str(
            r#"{"id":"vm-test","name":"app","os":"ubuntu-24.04","status":"stopped",
                "user":"virtum","hostname":"app","created_at":"2026-10-29T09:00:00Z"}"#,
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("virtum-diff-ssh-{}", std::process::id()));
        let diff = diff_machine(&lab, "app", &lab.machines["app"], &instance, &dir).unwrap();
        assert_eq!(
            diff.changes,
            vec![MachineChange::SshPort { from: 22, to: 2222 }]
        );
        assert_eq!(diff.changes[0].summary_line(), "ssh_port: 22 → 2222");
        assert!(!diff.changes[0].needs_apply());

        let restart = MachineChange::Restart {
            from: None,
            to: Some(RestartPolicy::UnlessStopped),
        };
        assert_eq!(
            restart.summary_line(),
            "restart: (non géré) → unless-stopped"
        );
        assert!(!restart.needs_apply());

        let mut aligned = instance.clone();
        aligned.ssh_port = Some(2222);
        let diff = diff_machine(&lab, "app", &lab.machines["app"], &aligned, &dir).unwrap();
        assert!(diff.changes.is_empty());
    }

    #[test]
    fn bind_drift_ignored_when_no_ports() {
        let lab = Virtumfile {
            name: "lab".into(),
            network: None,
            bridge_interface: None,
            bind: Some(PortBind::Lan),
            mounts: None,
            restart: None,
            ssh_port: None,
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
                    mounts: None,
                    restart: None,
                    ip: None,
                    ssh_port: None,
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
            mounts: None,
            restart: None,
            static_ip: None,
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
            mounts: None,
            restart: None,
            ssh_port: None,
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
                    mounts: None,
                    restart: None,
                    ip: None,
                    ssh_port: None,
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
            mounts: None,
            restart: None,
            static_ip: None,
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
