//! Lab orchestration via `Virtumfile` (`virtum up` / `virtum down`).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::config::{self, VirtumPaths};
use crate::lab_network;
use crate::libvirt::{domain_exists, domain_is_active, domain_state};
use crate::metadata::{self, Instance, InstanceStatus};
use crate::mounts;
use crate::port_forward::{self, PortBind, PortForward};
use crate::ssh::{ensure_ssh_keypair, read_public_key};
use crate::virtumfile;
use crate::vm::{self, cleanup_failed_run, lab_vm_id, launch_vm_inner, VmLaunchConfig};

pub fn up(file: Option<PathBuf>, insecure_ssh: bool, yes_lan: bool) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(up_async(file, insecure_ssh, yes_lan))
}

pub fn down(file: Option<PathBuf>, destroy: bool, purge: bool) -> Result<()> {
    if purge && !destroy {
        anyhow::bail!(
            "`--purge` nécessite `--destroy` (suppression des VM et des dossiers mounts hôte)."
        );
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(down_async(file, destroy, purge))
}

async fn up_async(file: Option<PathBuf>, insecure_ssh: bool, yes_lan: bool) -> Result<()> {
    let path = virtumfile::resolve_virtumfile_path(file)?;
    let vf_dir = virtumfile::virtumfile_dir(&path);
    let lab = virtumfile::load(&path)?;
    eprintln!(
        "Lab '{}' ({} machine(s)) depuis {}…",
        lab.name,
        lab.machines.len(),
        path.display()
    );

    if virtumfile::lab_requires_lan_consent(&lab)? && !yes_lan {
        confirm_lan_deploy()?;
    }

    let paths = config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;

    let lab_net = if virtumfile::lab_needs_isolated_network(&lab) {
        Some(lab_network::ensure_lab_network(&lab.name)?)
    } else {
        None
    };

    let port_plan: Vec<(String, Vec<PortForward>)> = lab
        .machines
        .iter()
        .map(|(name, spec)| {
            virtumfile::machine_port_forwards(spec).map(|forwards| (name.clone(), forwards))
        })
        .collect::<Result<_>>()?;
    for (machine_name, forwards) in &port_plan {
        if forwards.is_empty() {
            continue;
        }
        let bind = virtumfile::effective_port_bind(&lab, machine_name);
        let vm_id = lab_vm_id(&lab.name, machine_name);
        if metadata::find_by_lab_machine(&metadata, &lab.name, machine_name).is_some() {
            continue;
        }
        port_forward::ensure_ports_available(forwards, bind)?;
        port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), forwards)?;
    }

    for machine_name in lab.machines.keys() {
        let vm_id = lab_vm_id(&lab.name, machine_name);
        if metadata::find_by_lab_machine(&metadata, &lab.name, machine_name).is_some() {
            continue;
        }
        let instance_dir = paths.instances.join(&vm_id);
        let orphan_instance = instance_dir.join("disk.qcow2").is_file();
        if domain_exists(&vm_id) || orphan_instance {
            eprintln!("Nettoyage avant déploiement de '{machine_name}' ({vm_id})…");
            cleanup_failed_run(&vm_id, &instance_dir).await;
        }
        metadata::ensure_lab_machine_available(&metadata, &lab.name, machine_name)?;
    }

    let keys = ensure_ssh_keypair(&paths.keys)?;
    let public_key = read_public_key(&keys.public_key)?;

    let mut created_this_run: Vec<String> = Vec::new();
    let mut ready = 0usize;
    for (machine_name, spec) in &lab.machines {
        let machine = machine_name.as_str();
        let vm_id = lab_vm_id(&lab.name, machine);
        let metadata = metadata::load_metadata(&paths.metadata).await?;

        eprintln!("--- Machine '{machine_name}' ({vm_id}) ---");

        let spec_ports = virtumfile::machine_port_forwards(spec)?;
        let port_bind = virtumfile::effective_port_bind(&lab, machine);

        if let Some(existing) = metadata::find_by_lab_machine(&metadata, &lab.name, machine) {
            let instance_dir = paths.instances.join(&vm_id);
            if let Ok(diff) =
                crate::lab_diff::diff_machine(&lab, machine_name, spec, existing, &instance_dir)
            {
                if !diff.changes.is_empty() {
                    bail!(
                        "Le Virtumfile de la machine '{machine_name}' ne correspond plus à l'instance déployée. \
                         Appliquez les changements avec : virtum apply -f {}\n\
                         (ou `virtum down -f … --destroy` puis `virtum up` pour recréer).",
                        path.display()
                    );
                }
            }
            if !spec_ports.is_empty() {
                port_forward::ensure_ports_available(&spec_ports, port_bind)?;
                port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), &spec_ports)?;
            }
            let instance_dir = paths.instances.join(&vm_id);
            match resume_lab_machine(
                existing,
                &vm_id,
                machine_name,
                &instance_dir,
                &spec_ports,
                port_bind,
            )
            .await
            {
                Ok(()) => {
                    ready += 1;
                    continue;
                }
                Err(err) => {
                    eprintln!("Échec sur la machine '{machine_name}': {err:#}");
                    rollback_lab_vms(&paths, &created_this_run).await;
                    return Err(err);
                }
            }
        }

        let net_cfg = virtumfile::resolve_machine_network(&lab, machine, lab_net.as_deref())?;
        if net_cfg.mode == virtumfile::NetworkMode::Bridge {
            eprintln!(
                "  Réseau : bridge (LAN) sur {}",
                net_cfg.bridge_host_interface.as_deref().unwrap_or("?")
            );
        } else if net_cfg.bridge_host_interface.is_some() {
            eprintln!(
                "  Réseau : lab {} + bridge (LAN)",
                net_cfg.libvirt_network.as_deref().unwrap_or("?")
            );
        }
        let resolved_mounts = crate::mounts::resolve_machine_mounts(&lab, machine, &vf_dir)
            .with_context(|| format!("Machine '{machine_name}' : résolution des mounts"))?;
        let config = VmLaunchConfig {
            os: spec.os.clone(),
            ram: spec.ram,
            cpu: spec.cpu,
            user: "virtum".to_string(),
            name: Some(machine.to_string()),
            disk_size: spec.disk_size.clone(),
            provision: spec.provision.clone(),
            lab_name: Some(lab.name.clone()),
            machine_name: Some(machine.to_string()),
            port_forwards: spec_ports.clone(),
            port_bind,
            libvirt_network: net_cfg.libvirt_network,
            bridge_host_interface: net_cfg.bridge_host_interface,
            resolved_mounts,
            insecure_ssh,
        };
        let hostname = config.hostname_for_vm(&vm_id);
        let instance_dir = paths.instances.join(&vm_id);

        let mut retain = false;
        if let Err(err) = launch_vm_inner(
            &config,
            &paths,
            &instance_dir,
            &vm_id,
            &hostname,
            &public_key,
            &mut retain,
        )
        .await
        {
            eprintln!("Échec sur la machine '{machine_name}': {err:#}");
            cleanup_failed_run(&vm_id, &instance_dir).await;
            rollback_lab_vms(&paths, &created_this_run).await;
            return Err(err);
        }
        created_this_run.push(vm_id);
        ready += 1;
    }

    println!("Lab '{}' prêt ({} VM).", lab.name, ready);
    println!("  virtum ps");
    println!("  virtum shell <name>   # ex. virtum shell web");
    println!(
        "  virtum plan -f {}   # virtum apply si le Virtumfile a changé",
        path.display()
    );
    if crate::firewall::lab_needs_firewall_hint(&lab) {
        println!(
            "  Accès LAN (bind: lan) : ouvrez le pare-feu hôte avec :\n    \
             virtum open -f {}",
            path.display()
        );
    }
    Ok(())
}

fn confirm_lan_deploy() -> Result<()> {
    use std::io::{self, IsTerminal, Write};

    if !io::stdin().is_terminal() {
        bail!(
            "Refus du déploiement : bind: lan avec ports mappés sans terminal interactif. \
             Relancez avec --yes-lan si vous acceptez l'exposition sur le réseau local (0.0.0.0)."
        );
    }
    eprint!(
        "ATTENTION: bind: lan expose les ports mappés sur le réseau local (0.0.0.0). Continuer ? [y/N] "
    );
    io::stderr().flush().ok();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        eprintln!("\nDéploiement annulé.");
        std::process::exit(0);
    }
    let answer = line.trim();
    if answer == "y" || answer == "Y" {
        return Ok(());
    }
    eprintln!("Déploiement annulé.");
    std::process::exit(0);
}

/// Relance une machine de lab déjà enregistrée (après `virtum down` sans `--destroy`).
async fn resume_lab_machine(
    existing: &Instance,
    vm_id: &str,
    machine_name: &str,
    instance_dir: &std::path::Path,
    spec_ports: &[PortForward],
    port_bind: PortBind,
) -> Result<()> {
    if existing.id != vm_id {
        bail!(
            "Incohérence lab/machine '{machine_name}' : metadata {} vs domaine attendu {vm_id}. \
             Utilisez `virtum destroy {machine_name}` puis `virtum up`.",
            existing.id
        );
    }

    let reference = existing.name.clone().unwrap_or_else(|| existing.id.clone());

    if domain_exists(vm_id) {
        let state = domain_state(vm_id)?;
        if domain_is_active(&state) && existing.status == InstanceStatus::Running {
            let ip = existing.ip.as_deref().unwrap_or("-");
            eprintln!("Machine '{machine_name}' déjà en cours d'exécution (IP: {ip}).");
            if !spec_ports.is_empty() && ip != "-" {
                port_forward::start_instance_proxies(instance_dir, ip, spec_ports, port_bind)?;
            }
            port_forward::print_forward_lines(spec_ports, port_bind);
            println!("  SSH:      virtum shell {reference}");
            return Ok(());
        }
    }

    eprintln!("Relance de la machine '{machine_name}'…");
    vm::start_vm_async(&reference).await
}

async fn rollback_lab_vms(paths: &VirtumPaths, vm_ids: &[String]) {
    if vm_ids.is_empty() {
        return;
    }
    eprintln!("Nettoyage des machines déjà créées dans ce lab…");
    for vm_id in vm_ids {
        let instance_dir = paths.instances.join(vm_id);
        cleanup_failed_run(vm_id, &instance_dir).await;
        let metadata = metadata::load_metadata(&paths.metadata).await.ok();
        if let Some(meta) = metadata {
            if metadata::find_instance(&meta, vm_id).is_some() {
                let _ = metadata::remove_instance(&paths.metadata, vm_id).await;
            }
        }
    }
}

async fn down_async(file: Option<PathBuf>, destroy: bool, purge: bool) -> Result<()> {
    let path = virtumfile::resolve_virtumfile_path(file)?;
    let lab = virtumfile::load(&path)?;
    let vf_dir = virtumfile::virtumfile_dir(&path);
    let paths = config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    let instances = metadata::instances_for_lab(&metadata, &lab.name);

    if instances.is_empty() {
        eprintln!("Aucune VM active pour le lab '{}'.", lab.name);
        return Ok(());
    }

    let refs: Vec<String> = instances
        .iter()
        .map(|i| i.name.clone().unwrap_or_else(|| i.id.clone()))
        .collect();

    if destroy {
        eprintln!("Suppression de {} VM du lab '{}'…", refs.len(), lab.name);
        let purge_dirs = if purge {
            mounts::collect_host_dirs_for_lab(&lab, &vf_dir)?
        } else {
            Vec::new()
        };
        for reference in &refs {
            vm::destroy_vm_async(reference, false).await?;
        }
        if purge {
            let n = mounts::purge_host_mount_dirs(&purge_dirs, &vf_dir)?;
            if n > 0 {
                eprintln!(
                    "Dossiers mounts hôte supprimés ({n}) sous {}.",
                    vf_dir.display()
                );
            }
        }
        if virtumfile::lab_needs_isolated_network(&lab) {
            if let Err(err) = lab_network::destroy_lab_network(&lab.name) {
                eprintln!(
                    "Avertissement: impossible de supprimer le réseau du lab '{}': {err:#}",
                    lab.name
                );
            }
        }
        println!("Lab '{}' supprimé.", lab.name);
    } else {
        eprintln!("Arrêt de {} VM du lab '{}'…", refs.len(), lab.name);
        for reference in &refs {
            vm::stop_vm_async(reference).await?;
        }
        println!("Lab '{}' arrêté.", lab.name);
    }
    Ok(())
}
