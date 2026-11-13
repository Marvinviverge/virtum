//! Prévisualisation d'un déploiement lab (`virtum plan`, style Terraform plan).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::{self, VirtumPaths};
use crate::firewall;
use crate::images::resolve_image;
use crate::lab_network;
use crate::libvirt::{domain_exists, domain_is_active, domain_state};
use crate::metadata::{self, InstanceStatus};
use crate::port_forward::{self, PortBind, PortForward};
use crate::virtumfile::{self, NetworkMode, Virtumfile};
use crate::vm::lab_vm_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MachineAction {
    Create,
    Update,
    Resume,
    Unchanged,
}

#[derive(Debug, Clone)]
struct MachinePlan {
    name: String,
    vm_id: String,
    action: MachineAction,
    os: String,
    ram: u32,
    cpu: u32,
    network: NetworkMode,
    port_bind: PortBind,
    forwards: Vec<PortForward>,
    provision_steps: usize,
    static_ip: Option<String>,
    updates: Vec<String>,
}

pub fn plan(file: Option<PathBuf>) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(plan_async(file))
}

async fn plan_async(file: Option<PathBuf>) -> Result<()> {
    let virtumfile_path = virtumfile::resolve_virtumfile_path(file)?;
    let lab = virtumfile::load(&virtumfile_path)?;
    let paths = config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    errors.extend(crate::lab_diff::collect_lab_blocking_errors(
        &lab,
        &virtumfile_path,
        &metadata,
    ));
    warnings.extend(crate::lab_diff::collect_mount_warnings(&lab, &metadata));

    let port_plan: Vec<(String, Vec<PortForward>)> = lab
        .machines
        .iter()
        .map(|(name, spec)| {
            virtumfile::machine_port_forwards(spec).map(|forwards| (name.clone(), forwards))
        })
        .collect::<Result<_>>()?;

    if let Err(e) = port_forward::validate_lab_port_uniqueness(&port_plan) {
        errors.push(e.to_string());
    }
    if let Ok(net) = crate::lab_network::lab_network_name(&lab.name) {
        let owners = crate::lab_network::lab_instance_macs(
            &paths.instances,
            &metadata::instances_for_lab(&metadata, &lab.name),
        );
        let (ip_errors, ip_warnings) = crate::lab::declared_ip_checks(&lab, &net, &owners);
        errors.extend(ip_errors);
        warnings.extend(ip_warnings);
    }
    for (machine_name, spec) in &lab.machines {
        let declared = virtumfile::machine_static_ip(spec).map(|ip| ip.to_string());
        let Some(inst) = metadata::find_by_lab_machine(&metadata, &lab.name, machine_name) else {
            continue;
        };
        if let (Some(want), Some(have)) = (inst.static_ip.as_ref(), inst.ip.as_ref()) {
            if declared.as_ref() == Some(want) && want != have {
                warnings.push(format!(
                    "Machine '{machine_name}' : adresse actuelle {have}, déclarée {want} \
                     (ancien bail ?). `virtum stop {machine_name}` / `virtum start {machine_name}` \
                     une fois le bail expiré."
                ));
            }
        }
    }

    let needs_socat = port_plan.iter().any(|(_, f)| !f.is_empty());
    let any_create = lab
        .machines
        .keys()
        .any(|m| metadata::find_by_lab_machine(&metadata, &lab.name, m).is_none());

    for (machine_name, forwards) in &port_plan {
        let bind = virtumfile::effective_port_bind(&lab, machine_name);
        let vm_id = lab_vm_id(&lab.name, machine_name);
        let is_new = metadata::find_by_lab_machine(&metadata, &lab.name, machine_name).is_none();

        if is_new && !forwards.is_empty() {
            if let Err(e) = port_forward::ensure_ports_available(forwards, bind) {
                errors.push(format!("Machine '{machine_name}': {e:#}"));
            }
            if let Err(e) =
                port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), forwards)
            {
                errors.push(format!("Machine '{machine_name}': {e:#}"));
            }
        }

        if let Some(net) = effective_network_label(&lab, machine_name) {
            if net.contains("bridge") && !forwards.is_empty() && bind == PortBind::Local {
                warnings.push(format!(
                    "Machine '{machine_name}' : `network: bridge` avec `ports:` en bind local — \
                     l'IP LAN directe suffit souvent ; `bind: lan` si vous passez par socat."
                ));
            }
        }
    }

    for machine_name in lab.machines.keys() {
        if metadata::find_by_lab_machine(&metadata, &lab.name, machine_name).is_some() {
            continue;
        }
        let vm_id = lab_vm_id(&lab.name, machine_name);
        if domain_exists(&vm_id) {
            warnings.push(format!(
                "Domaine orphelin '{vm_id}' (machine '{machine_name}') : `virtum up` le nettoiera avant création."
            ));
        }
        if let Err(e) = metadata::ensure_lab_machine_available(&metadata, &lab.name, machine_name) {
            errors.push(e.to_string());
        }
    }

    let mut machines: Vec<MachinePlan> = Vec::new();
    for (machine_name, spec) in &lab.machines {
        let vm_id = lab_vm_id(&lab.name, machine_name);
        let forwards = virtumfile::machine_port_forwards(spec)?;
        let port_bind = virtumfile::effective_port_bind(&lab, machine_name);
        let network = virtumfile::effective_network_mode(&lab, machine_name);

        let instance_dir = paths.instances.join(&vm_id);
        let mut updates = Vec::new();
        let action = match metadata::find_by_lab_machine(&metadata, &lab.name, machine_name) {
            None => MachineAction::Create,
            Some(inst) => {
                if inst.id != vm_id {
                    errors.push(format!(
                        "Machine '{machine_name}' : metadata {} ≠ domaine attendu {vm_id}. \
                         `virtum destroy {machine_name}` puis `virtum up`.",
                        inst.id
                    ));
                }
                if let Ok(diff) =
                    crate::lab_diff::diff_machine(&lab, machine_name, spec, inst, &instance_dir)
                {
                    for c in &diff.changes {
                        if c.is_blocking() {
                            errors.push(format!(
                                "Machine '{machine_name}' : {} — utilisez destroy + up.",
                                c.summary_line()
                            ));
                        } else {
                            updates.push(c.summary_line());
                        }
                    }
                }
                if !updates.is_empty() {
                    MachineAction::Update
                } else if domain_exists(&vm_id) {
                    // Lecture seule : statut réel libvirt, sans réécrire metadata.json.
                    let active = domain_state(&vm_id).ok().map(|s| domain_is_active(&s));
                    if metadata::reconcile_status(inst.status, active) == InstanceStatus::Running {
                        MachineAction::Unchanged
                    } else {
                        MachineAction::Resume
                    }
                } else {
                    MachineAction::Resume
                }
            }
        };

        machines.push(MachinePlan {
            name: machine_name.clone(),
            vm_id,
            action,
            os: spec.os.clone(),
            ram: spec.ram,
            cpu: spec.cpu,
            network,
            port_bind,
            forwards,
            provision_steps: spec.provision.len(),
            static_ip: virtumfile::machine_static_ip(spec).map(|ip| ip.to_string()),
            updates,
        });
    }
    machines.sort_by(|a, b| a.name.cmp(&b.name));

    print_header(&virtumfile_path, &lab, &paths);

    if virtumfile::lab_needs_isolated_network(&lab) {
        let has_vms = !metadata::instances_for_lab(&metadata, &lab.name).is_empty();
        print_lab_network(&lab, has_vms, &mut errors)?;
    } else {
        println!("Réseau lab : (non créé — lab entier en `network: bridge`)");
    }

    println!("\nMachines :");
    let mut create_n = 0usize;
    let mut update_n = 0usize;
    let mut resume_n = 0usize;
    let mut unchanged_n = 0usize;
    for m in &machines {
        let sym = match m.action {
            MachineAction::Create => {
                create_n += 1;
                "+"
            }
            MachineAction::Update => {
                update_n += 1;
                "~"
            }
            MachineAction::Resume => {
                resume_n += 1;
                "~"
            }
            MachineAction::Unchanged => {
                unchanged_n += 1;
                "="
            }
        };
        let action_label = match m.action {
            MachineAction::Create => "create",
            MachineAction::Update => "update",
            MachineAction::Resume => "resume",
            MachineAction::Unchanged => "unchanged",
        };
        let net = network_mode_label(m.network);
        let bind = match m.port_bind {
            PortBind::Local => "local",
            PortBind::Lan => "lan",
        };
        let ports = if m.forwards.is_empty() {
            "-".to_string()
        } else {
            m.forwards
                .iter()
                .map(|p| format!("{}:{}", p.host_port, p.guest_port))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "  {sym} {action_label:<9} {:<12} ({})  os={} ram={} cpu={} network={} bind={} ports=[{ports}]",
            m.name, m.vm_id, m.os, m.ram, m.cpu, net, bind
        );
        if let Some(ref ip) = m.static_ip {
            println!("      ip: {ip} (déclarée)");
        }
        if m.provision_steps > 0 {
            println!("      provision: {} commande(s)", m.provision_steps);
        }
        let mount_list = crate::mounts::format_mount_list(
            &crate::virtumfile::effective_mount_specs(&lab, &m.name),
        );
        if mount_list != "-" {
            println!("      mounts: {mount_list}");
        }
        for line in &m.updates {
            println!("      → {line}");
        }
        let inst_dir = paths.instances.join(&m.vm_id);
        println!("      instance: {}", inst_dir.display());
    }

    let removals = crate::lab_diff::collect_removals(&lab, &metadata, &paths.instances);
    for r in &removals {
        println!(
            "  - remove    {:<12} ({})  hors du Virtumfile — supprimée par `virtum apply --prune`",
            r.machine_name, r.vm_id
        );
        if r.snapshots > 0 {
            println!("      {} instantané(s) virtum-* seront perdus", r.snapshots);
        }
    }
    let remove_n = removals.len();

    print_images(&paths, &lab, &mut warnings)?;

    if needs_socat && !command_exists("socat") {
        errors.push(
            "socat est introuvable (requis pour `ports:`). Installez-le ou lancez `virtum doctor`."
                .to_string(),
        );
    }

    if any_create && !command_exists("cloud-localds") {
        errors.push(
            "cloud-localds est introuvable (création de nouvelles VM). \
             Installez cloud-image-utils ou `virtum doctor`."
                .to_string(),
        );
    }

    if virtumfile::lab_uses_bridge(&lab) {
        let explicit = virtumfile::lab_bridge_interface_for_validation(&lab);
        let cap = crate::bridge_network::scan_bridge_capability();
        for w in &cap.warnings {
            warnings.push(w.clone());
        }
        if let Some(iface) = explicit.or(cap.recommended_interface.as_deref()) {
            println!("\nBridge LAN : interface cible « {iface} » (voir `virtum doctor`).");
        }
        if virtumfile::lab_has_bridge_lab_mix(&lab) {
            println!(
                "  Mode hybride : machines bridge + NIC lab `virtum-lab-{}` (dual NIC).",
                lab.name
            );
        } else if lab.network == Some(NetworkMode::Bridge) {
            println!("  Lab entier en bridge : pas de réseau virtum-lab-*.");
        }
    }

    if let Some(ref spec) = lab.ansible {
        if let Err(e) = crate::ansible::require_tools(spec) {
            errors.push(format!("{e:#}"));
        }
        let (status, will_run) = match crate::ansible::load_state(&paths, &lab.name)? {
            Some(state) if state.succeeded() => {
                let when = state.provisioned_at.unwrap_or_default();
                if any_create {
                    (
                        format!("provisionné ({when}) ; sera rejoué au up (machines à créer)"),
                        true,
                    )
                } else {
                    (
                        format!("déjà provisionné ({when}) — `virtum provision` pour rejouer"),
                        false,
                    )
                }
            }
            Some(_) => (
                "dernier provisionnement en échec — sera rejoué au up".to_string(),
                true,
            ),
            None => ("sera joué au up (lab non provisionné)".to_string(), true),
        };
        // L'instantané n'est pris qu'après un provisionnement joué par `up`.
        let snapshot = spec
            .snapshot_name()
            .filter(|_| will_run)
            .map(|n| format!(", puis instantané '{n}'"))
            .unwrap_or_default();
        println!("\nAnsible : {} — {status}{snapshot}", spec.playbook);
    }

    collect_bind_lan_warnings(&paths, &lab, &virtumfile_path, &mut warnings)?;

    if !warnings.is_empty() {
        println!("\nAvertissements :");
        for w in &warnings {
            println!("  ! {w}");
        }
    }

    if !errors.is_empty() {
        println!("\nErreurs (bloquent `virtum up`) :");
        for e in &errors {
            println!("  ✗ {e}");
        }
    }

    println!(
        "\nRésumé : {create_n} à créer, {update_n} à mettre à jour (apply), {resume_n} à reprendre, {unchanged_n} inchangée(s), \
         {remove_n} hors du Virtumfile. {} avertissement(s), {} erreur(s).",
        warnings.len(),
        errors.len()
    );

    if !errors.is_empty() {
        bail!(
            "Le plan contient {} erreur(s). Corrigez le Virtumfile ou l'hôte, puis relancez `virtum plan`.",
            errors.len()
        );
    }

    if update_n > 0 {
        println!(
            "Modifications détectées : virtum apply -f {}",
            virtumfile_path.display()
        );
    }
    if remove_n > 0 {
        println!(
            "Machines hors du Virtumfile : virtum apply --prune -f {}",
            virtumfile_path.display()
        );
    }
    if create_n > 0 || resume_n > 0 {
        println!(
            "Déploiement / reprise : virtum up -f {}",
            virtumfile_path.display()
        );
    }
    Ok(())
}

fn print_header(path: &Path, lab: &Virtumfile, paths: &VirtumPaths) {
    println!(
        "Virtum plan — lab '{}' ({} machine(s))",
        lab.name,
        lab.machines.len()
    );
    println!("  Virtumfile : {}", path.display());
    println!("  État       : {}", paths.root.display());
    let bind_lab = lab.bind.map(|b| match b {
        PortBind::Local => "local",
        PortBind::Lan => "lan",
    });
    if let Some(b) = bind_lab {
        println!("  bind (lab) : {b}");
    }
    if lab.network.is_some() {
        println!(
            "  network (lab) : {}",
            network_mode_label(lab.network.unwrap())
        );
    }
}

/// Ligne « Réseau lab » ; erreurs : chevauchement avec un autre réseau, ou sous-réseau
/// changé alors que des VM du lab existent.
fn print_lab_network(lab: &Virtumfile, has_vms: bool, errors: &mut Vec<String>) -> Result<()> {
    let net_name = lab_network::lab_network_name(&lab.name)?;
    let subnet = lab_network::lab_subnet(lab);
    let defined = lab_network::lab_network_is_defined(&net_name)?;
    let active = if defined {
        lab_network::lab_network_is_active(&net_name)?
    } else {
        false
    };
    let mut status = if active {
        "actif".to_string()
    } else if defined {
        "défini, inactif (sera démarré au up)".to_string()
    } else {
        "à créer au up".to_string()
    };
    if let Some((addr, len)) = lab_network::defined_subnet(&net_name)? {
        if !(len == 24 && subnet.contains(addr)) {
            if has_vms {
                errors.push(format!(
                    "Le réseau {net_name} existe en {addr}/{len}, le Virtumfile demande {subnet} : \
                     `virtum down -f … --destroy` puis `virtum up` pour changer de sous-réseau."
                ));
            } else {
                status = format!("existe en {addr}/{len}, sera recréé au up (aucune VM)");
            }
        }
    }
    errors.extend(lab_network::subnet_conflicts(lab)?);
    let origin = if lab.subnet.is_some() {
        "déclaré"
    } else {
        "calculé"
    };
    println!("Réseau lab : {net_name} (NAT {subnet}, {origin}) — {status}");
    Ok(())
}

fn print_images(paths: &VirtumPaths, lab: &Virtumfile, warnings: &mut Vec<String>) -> Result<()> {
    let mut os_set: Vec<String> = lab.machines.values().map(|s| s.os.clone()).collect();
    os_set.sort();
    os_set.dedup();

    println!("\nImages :");
    let mut missing = Vec::new();
    for os in &os_set {
        let image = resolve_image(os, &paths.images)?;
        if image.local_path.is_file() {
            println!("  ok   {}  {}", os, image.local_path.display());
        } else {
            println!("  !    {}  manquante", os);
            missing.push(os.clone());
            warnings.push(format!(
                "Image '{os}' absente — téléchargez avec : virtum pull {os}"
            ));
        }
    }
    if !missing.is_empty() && missing.len() == os_set.len() {
        println!("  → virtum pull <os> pour chaque OS listé ci-dessus.");
    }
    Ok(())
}

fn collect_bind_lan_warnings(
    paths: &VirtumPaths,
    lab: &Virtumfile,
    virtumfile_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let rules = virtumfile::collect_lan_firewall_rules(lab)?;
    if rules.is_empty() {
        return Ok(());
    }

    let ports: Vec<String> = rules
        .iter()
        .map(|r| format!("{}/tcp", r.host_port))
        .collect();
    println!(
        "\nPorts bind: lan : {} (socat 0.0.0.0)",
        rules
            .iter()
            .map(|r| format!("{}:{}→{}", r.machine, r.host_port, r.guest_port))
            .collect::<Vec<_>>()
            .join(", ")
    );
    warnings.push(
        "Sécurité réseau : bind: lan expose les ports mappés sur toutes les interfaces (0.0.0.0) \
         — accessible depuis le réseau local. `virtum up` demandera une confirmation."
            .to_string(),
    );

    match firewall::get_firewall_state(paths, &lab.name)? {
        None => {
            warnings.push(format!(
                "Pare-feu hôte : aucun `virtum open` enregistré pour le lab '{}' (ports Virtumfile : {}). \
                 Après `virtum up` : virtum open -f {}",
                lab.name,
                ports.join(", "),
                virtumfile_path.display()
            ));
        }
        Some(state) => {
            let desired_ports = firewall::lan_host_ports(&rules);
            let stored_ports = firewall::lan_host_ports(&state.rules);
            if firewall::firewall_ports_aligned(&rules, &state.rules) {
                println!(
                    "  Pare-feu : `virtum open` aligné avec le Virtumfile ({})",
                    firewall::format_ports_tcp(&desired_ports)
                );
            } else {
                warnings.push(format!(
                    "Pare-feu hôte : `virtum open` enregistre {} mais le Virtumfile demande {}. \
                     Lancez : virtum close -f {} puis virtum open -f {}",
                    firewall::format_ports_tcp(&stored_ports),
                    firewall::format_ports_tcp(&desired_ports),
                    virtumfile_path.display(),
                    virtumfile_path.display()
                ));
            }
        }
    }

    if !command_exists("ufw") {
        warnings.push(
            "ufw introuvable — ouvrez les ports LAN manuellement ou installez ufw.".to_string(),
        );
    } else if !firewall::is_ufw_enabled() {
        warnings.push(
            "ufw n'est pas activé (ENABLED=no dans /etc/ufw/ufw.conf). \
             Requis pour `virtum open` : sudo ufw enable"
                .to_string(),
        );
    }

    Ok(())
}

fn effective_network_label(lab: &Virtumfile, machine_name: &str) -> Option<String> {
    let mode = virtumfile::effective_network_mode(lab, machine_name);
    if lab.network == Some(NetworkMode::Bridge) || mode == NetworkMode::Bridge {
        return Some("bridge".to_string());
    }
    if virtumfile::lab_has_bridge_lab_mix(lab) && mode == NetworkMode::Bridge {
        return Some("bridge+lab".to_string());
    }
    None
}

fn network_mode_label(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::Lab => "lab",
        NetworkMode::Bridge => "bridge",
    }
}

fn command_exists(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_exists_is_bool() {
        let _ = command_exists("false-binary-virtum-test");
    }
}
