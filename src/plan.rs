//! Prévisualisation d'un déploiement lab (`virtum plan`, style Terraform plan).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::{self, VirtumPaths};
use crate::firewall;
use crate::images::resolve_image;
use crate::lab_network;
use crate::libvirt::{domain_exists, domain_is_active, domain_state};
use crate::metadata::{self, InstanceStatus};
use crate::port_forward::{self, PortBind, PortForward};
use crate::tr;
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
        .context(tr!(
            fr: "Impossible de démarrer le runtime Tokio",
            en: "Cannot start the Tokio runtime"
        ))?
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
                warnings.push(tr!(
                    fr: "Machine '{machine_name}' : adresse actuelle {have}, déclarée {want} \
                     (ancien bail ?). `virtum stop {machine_name}` / `virtum start {machine_name}` \
                     une fois le bail expiré.",
                    en: "Machine '{machine_name}': current address {have}, declared {want} \
                     (old lease?). Run `virtum stop {machine_name}` / \
                     `virtum start {machine_name}` once the lease has expired."
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
                warnings.push(tr!(
                    fr: "Machine '{machine_name}' : `network: bridge` avec `ports:` en bind \
                         local — \
                     l'IP LAN directe suffit souvent ; `bind: lan` si vous passez par socat.",
                    en: "Machine '{machine_name}': `network: bridge` with `ports:` bound locally — \
                     the direct LAN IP is often enough; use `bind: lan` if you go through socat."
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
            warnings.push(tr!(
                fr: "Domaine orphelin '{vm_id}' (machine '{machine_name}') : \
                     `virtum up` le nettoiera avant création.",
                en: "Orphan domain '{vm_id}' (machine '{machine_name}'): \
                     `virtum up` will clean it up before creation."
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
                    let inst_id = &inst.id;
                    errors.push(tr!(
                        fr: "Machine '{machine_name}' : metadata {inst_id} ≠ domaine attendu \
                             {vm_id}. \
                         `virtum destroy {machine_name}` puis `virtum up`.",
                        en: "Machine '{machine_name}': metadata {inst_id} ≠ expected domain \
                             {vm_id}. \
                         Run `virtum destroy {machine_name}` then `virtum up`."
                    ));
                }
                if let Ok(diff) =
                    crate::lab_diff::diff_machine(&lab, machine_name, spec, inst, &instance_dir)
                {
                    for c in &diff.changes {
                        if c.is_blocking() {
                            let summary = c.summary_line();
                            errors.push(tr!(
                                fr: "Machine '{machine_name}' : {summary} — utilisez destroy + up.",
                                en: "Machine '{machine_name}': {summary} — use destroy + up."
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
        println!(
            "{}",
            tr!(
                fr: "Réseau lab : (non créé — lab entier en `network: bridge`)",
                en: "Lab network: (not created — whole lab in `network: bridge`)"
            )
        );
    }

    println!("{}", tr!(fr: "\nMachines :", en: "\nMachines:"));
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
            "  {sym} {action_label:<9} {:<12} ({})  os={} ram={} cpu={} network={} bind={} ports=[{ports}]", // i18n-ok : identique en anglais
            m.name, m.vm_id, m.os, m.ram, m.cpu, net, bind
        );
        if let Some(ref ip) = m.static_ip {
            println!(
                "{}",
                tr!(fr: "      ip: {ip} (déclarée)", en: "      ip: {ip} (declared)")
            );
        }
        if m.provision_steps > 0 {
            let steps = m.provision_steps;
            println!(
                "{}",
                tr!(
                    fr: "      provision: {steps} commande(s)",
                    en: "      provision: {steps} command(s)"
                )
            );
        }
        let mount_list = crate::mounts::format_mount_list(
            &crate::virtumfile::effective_mount_specs(&lab, &m.name),
        );
        if mount_list != "-" {
            println!("      mounts: {mount_list}"); // i18n-ok : identique en anglais
        }
        for line in &m.updates {
            println!("      → {line}");
        }
        let inst_dir = paths.instances.join(&m.vm_id);
        println!("      instance: {}", inst_dir.display()); // i18n-ok : identique en anglais
    }

    let removals = crate::lab_diff::collect_removals(&lab, &metadata, &paths.instances);
    for r in &removals {
        println!(
            "{}",
            tr!(
                fr: "  - remove    {:<12} ({})  hors du Virtumfile — supprimée par \
                     `virtum apply --prune`",
                en: "  - remove    {:<12} ({})  not in the Virtumfile — removed by \
                     `virtum apply --prune`",
                r.machine_name,
                r.vm_id
            )
        );
        if r.snapshots > 0 {
            let snapshots = r.snapshots;
            println!(
                "{}",
                tr!(
                    fr: "      {snapshots} instantané(s) virtum-* seront perdus",
                    en: "      {snapshots} virtum-* snapshot(s) will be lost"
                )
            );
        }
    }
    let remove_n = removals.len();

    print_images(&paths, &lab, &mut warnings)?;

    if needs_socat && !command_exists("socat") {
        errors.push(tr!(
            fr: "socat est introuvable (requis pour `ports:`). Installez-le \
                 ou lancez `virtum doctor`.",
            en: "socat not found (required for `ports:`). Install it or run `virtum doctor`."
        ));
    }

    if any_create && !command_exists("cloud-localds") {
        errors.push(tr!(
            fr: "cloud-localds est introuvable (création de nouvelles VM). \
             Installez cloud-image-utils ou `virtum doctor`.",
            en: "cloud-localds not found (needed to create new VMs). \
             Install cloud-image-utils or run `virtum doctor`."
        ));
    }

    if virtumfile::lab_uses_bridge(&lab) {
        let explicit = virtumfile::lab_bridge_interface_for_validation(&lab);
        let cap = crate::bridge_network::scan_bridge_capability();
        for w in &cap.warnings {
            warnings.push(w.clone());
        }
        if let Some(iface) = explicit.or(cap.recommended_interface.as_deref()) {
            println!(
                "{}",
                tr!(
                    fr: "\nBridge LAN : interface cible « {iface} » (voir `virtum doctor`).",
                    en: "\nLAN bridge: target interface '{iface}' (see `virtum doctor`)."
                )
            );
        }
        if virtumfile::lab_has_bridge_lab_mix(&lab) {
            let lab_name = &lab.name;
            println!(
                "{}",
                tr!(
                    fr: "  Mode hybride : machines bridge + NIC lab `virtum-lab-{lab_name}` \
                         (dual NIC).",
                    en: "  Hybrid mode: bridge machines + lab NIC `virtum-lab-{lab_name}` (dual \
                         NIC)."
                )
            );
        } else if lab.network == Some(NetworkMode::Bridge) {
            println!(
                "{}",
                tr!(
                    fr: "  Lab entier en bridge : pas de réseau virtum-lab-*.",
                    en: "  Whole lab in bridge mode: no virtum-lab-* network."
                )
            );
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
                        tr!(
                            fr: "provisionné ({when}) ; sera rejoué au up (machines à créer)",
                            en: "provisioned ({when}); will be replayed on up (machines to create)"
                        ),
                        true,
                    )
                } else {
                    (
                        tr!(
                            fr: "déjà provisionné ({when}) — `virtum provision` pour rejouer",
                            en: "already provisioned ({when}) — run `virtum provision` to replay"
                        ),
                        false,
                    )
                }
            }
            Some(_) => (
                tr!(
                    fr: "dernier provisionnement en échec — sera rejoué au up",
                    en: "last provisioning failed — will be replayed on up"
                ),
                true,
            ),
            None => (
                tr!(
                    fr: "sera joué au up (lab non provisionné)",
                    en: "will run on up (lab not provisioned)"
                ),
                true,
            ),
        };
        // L'instantané n'est pris qu'après un provisionnement joué par `up`.
        let snapshot = spec
            .snapshot_name()
            .filter(|_| will_run)
            .map(|n| tr!(fr: ", puis instantané '{n}'", en: ", then snapshot '{n}'"))
            .unwrap_or_default();
        let playbook = &spec.playbook;
        println!(
            "{}",
            tr!(
                fr: "\nAnsible : {playbook} — {status}{snapshot}",
                en: "\nAnsible: {playbook} — {status}{snapshot}"
            )
        );
    }

    collect_bind_lan_warnings(&paths, &lab, &virtumfile_path, &mut warnings)?;

    if !warnings.is_empty() {
        println!("{}", tr!(fr: "\nAvertissements :", en: "\nWarnings:"));
        for w in &warnings {
            println!("  ! {w}");
        }
    }

    if !errors.is_empty() {
        println!(
            "{}",
            tr!(fr: "\nErreurs (bloquent `virtum up`) :", en: "\nErrors (block `virtum up`):")
        );
        for e in &errors {
            println!("  ✗ {e}");
        }
    }

    println!(
        "{}",
        tr!(
            fr: "\nRésumé : {create_n} à créer, {update_n} à mettre à jour (apply), \
                 {resume_n} à reprendre, {unchanged_n} inchangée(s), \
                 {remove_n} hors du Virtumfile. {} avertissement(s), {} erreur(s).",
            en: "\nSummary: {create_n} to create, {update_n} to update (apply), \
                 {resume_n} to resume, {unchanged_n} unchanged, \
                 {remove_n} not in the Virtumfile. {} warning(s), {} error(s).",
            warnings.len(),
            errors.len()
        )
    );

    if !errors.is_empty() {
        bail!(tr!(
            fr: "Le plan contient {} erreur(s). Corrigez le Virtumfile ou l'hôte, \
                 puis relancez `virtum plan`.",
            en: "The plan has {} error(s). Fix the Virtumfile or the host, \
                 then run `virtum plan` again.",
            errors.len()
        ));
    }

    let vf = virtumfile_path.display();
    if update_n > 0 {
        println!(
            "{}",
            tr!(
                fr: "Modifications détectées : virtum apply -f {vf}",
                en: "Changes detected: virtum apply -f {vf}"
            )
        );
    }
    if remove_n > 0 {
        println!(
            "{}",
            tr!(
                fr: "Machines hors du Virtumfile : virtum apply --prune -f {vf}",
                en: "Machines not in the Virtumfile: virtum apply --prune -f {vf}"
            )
        );
    }
    if create_n > 0 || resume_n > 0 {
        println!(
            "{}",
            tr!(
                fr: "Déploiement / reprise : virtum up -f {vf}",
                en: "Deploy / resume: virtum up -f {vf}"
            )
        );
    }
    Ok(())
}

fn print_header(path: &Path, lab: &Virtumfile, paths: &VirtumPaths) {
    println!(
        "Virtum plan — lab '{}' ({} machine(s))", // i18n-ok : identique en anglais
        lab.name,
        lab.machines.len()
    );
    let path = path.display();
    let root = paths.root.display();
    println!(
        "{}",
        tr!(fr: "  Virtumfile : {path}", en: "  Virtumfile:    {path}")
    );
    println!(
        "{}",
        tr!(fr: "  État       : {root}", en: "  State:         {root}")
    );
    let bind_lab = lab.bind.map(|b| match b {
        PortBind::Local => "local",
        PortBind::Lan => "lan",
    });
    if let Some(b) = bind_lab {
        println!(
            "{}",
            tr!(fr: "  bind (lab) : {b}", en: "  bind (lab):    {b}")
        );
    }
    if let Some(network) = lab.network {
        let label = network_mode_label(network);
        println!(
            "{}",
            tr!(fr: "  network (lab) : {label}", en: "  network (lab): {label}")
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
        tr!(fr: "actif", en: "active")
    } else if defined {
        tr!(
            fr: "défini, inactif (sera démarré au up)",
            en: "defined, inactive (will be started on up)"
        )
    } else {
        tr!(fr: "à créer au up", en: "to be created on up")
    };
    if let Some((addr, len)) = lab_network::defined_subnet(&net_name)? {
        if !(len == 24 && subnet.contains(addr)) {
            if has_vms {
                errors.push(tr!(
                    fr: "Le réseau {net_name} existe en {addr}/{len}, le Virtumfile demande \
                         {subnet} : \
                     `virtum down -f … --destroy` puis `virtum up` pour changer de sous-réseau.",
                    en: "Network {net_name} exists as {addr}/{len}, the Virtumfile asks for \
                         {subnet}: \
                     run `virtum down -f … --destroy` then `virtum up` to change the subnet."
                ));
            } else {
                status = tr!(
                    fr: "existe en {addr}/{len}, sera recréé au up (aucune VM)",
                    en: "exists as {addr}/{len}, will be recreated on up (no VM)"
                );
            }
        }
    }
    errors.extend(lab_network::subnet_conflicts(lab)?);
    let origin = if lab.subnet.is_some() {
        tr!(fr: "déclaré", en: "declared")
    } else {
        tr!(fr: "calculé", en: "computed")
    };
    println!(
        "{}",
        tr!(
            fr: "Réseau lab : {net_name} (NAT {subnet}, {origin}) — {status}",
            en: "Lab network: {net_name} (NAT {subnet}, {origin}) — {status}"
        )
    );
    Ok(())
}

fn print_images(paths: &VirtumPaths, lab: &Virtumfile, warnings: &mut Vec<String>) -> Result<()> {
    let mut os_set: Vec<String> = lab.machines.values().map(|s| s.os.clone()).collect();
    os_set.sort();
    os_set.dedup();

    println!("{}", tr!(fr: "\nImages :", en: "\nImages:"));
    let mut missing = Vec::new();
    for os in &os_set {
        let image = resolve_image(os, &paths.images)?;
        if image.local_path.is_file() {
            println!("  ok   {}  {}", os, image.local_path.display()); // i18n-ok
        } else {
            println!(
                "{}",
                tr!(fr: "  !    {}  manquante", en: "  !    {}  missing", os)
            );
            missing.push(os.clone());
            warnings.push(tr!(
                fr: "Image '{os}' absente — téléchargez avec : virtum pull {os}",
                en: "Image '{os}' missing — download it with: virtum pull {os}"
            ));
        }
    }
    if !missing.is_empty() && missing.len() == os_set.len() {
        println!(
            "{}",
            tr!(
                fr: "  → virtum pull <os> pour chaque OS listé ci-dessus.",
                en: "  → virtum pull <os> for each OS listed above."
            )
        );
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
    let lan_list = rules
        .iter()
        .map(|r| format!("{}:{}→{}", r.machine, r.host_port, r.guest_port))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "{}",
        tr!(
            fr: "\nPorts bind: lan : {lan_list} (socat 0.0.0.0)",
            en: "\nPorts bind: lan: {lan_list} (socat 0.0.0.0)"
        )
    );
    warnings.push(tr!(
        fr: "Sécurité réseau : bind: lan expose les ports mappés sur toutes les interfaces \
             (0.0.0.0) \
         — accessible depuis le réseau local. `virtum up` et `virtum apply` demanderont une \
         confirmation (ou --yes-lan).",
        en: "Network security: bind: lan exposes the mapped ports on all interfaces (0.0.0.0) \
         — reachable from the local network. `virtum up` and `virtum apply` will ask for \
         confirmation (or --yes-lan)."
    ));

    match firewall::get_firewall_state(paths, &lab.name)? {
        None => {
            let lab_name = &lab.name;
            let ports = ports.join(", ");
            let vf = virtumfile_path.display();
            warnings.push(tr!(
                fr: "Pare-feu hôte : aucun `virtum open` enregistré pour le lab '{lab_name}' \
                 (ports Virtumfile : {ports}). Après `virtum up` : virtum open -f {vf}",
                en: "Host firewall: no `virtum open` recorded for lab '{lab_name}' \
                 (Virtumfile ports: {ports}). After `virtum up`: virtum open -f {vf}"
            ));
        }
        Some(state) => {
            let desired_ports = firewall::lan_host_ports(&rules);
            let stored_ports = firewall::lan_host_ports(&state.rules);
            if firewall::firewall_ports_aligned(&rules, &state.rules) {
                let desired = firewall::format_ports_tcp(&desired_ports);
                println!(
                    "{}",
                    tr!(
                        fr: "  Pare-feu : `virtum open` aligné avec le Virtumfile ({desired})",
                        en: "  Firewall: `virtum open` matches the Virtumfile ({desired})"
                    )
                );
            } else {
                let stored = firewall::format_ports_tcp(&stored_ports);
                let desired = firewall::format_ports_tcp(&desired_ports);
                let vf = virtumfile_path.display();
                warnings.push(tr!(
                    fr: "Pare-feu hôte : `virtum open` enregistre {stored} mais le Virtumfile \
                     demande {desired}. Lancez : virtum close -f {vf} puis virtum open -f {vf}",
                    en: "Host firewall: `virtum open` records {stored} but the Virtumfile \
                     asks for {desired}. Run: virtum close -f {vf} then virtum open -f {vf}"
                ));
            }
        }
    }

    if !command_exists("ufw") {
        warnings.push(tr!(
            fr: "ufw introuvable — ouvrez les ports LAN manuellement ou installez ufw.",
            en: "ufw not found — open the LAN ports manually or install ufw."
        ));
    } else if !firewall::is_ufw_enabled() {
        warnings.push(tr!(
            fr: "ufw n'est pas activé (ENABLED=no dans /etc/ufw/ufw.conf). \
             Requis pour `virtum open` : sudo ufw enable",
            en: "ufw is not enabled (ENABLED=no in /etc/ufw/ufw.conf). \
             Required for `virtum open`: sudo ufw enable"
        ));
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

use crate::command::command_exists;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_exists_is_bool() {
        let _ = command_exists("false-binary-virtum-test");
    }
}
