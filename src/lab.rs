//! Lab orchestration via `Virtumfile` (`virtum up` / `virtum down`).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::config::{self, VirtumPaths};
use crate::lab_network;
use crate::libvirt::{domain_exists, domain_is_active, domain_state};
use crate::metadata::{self, Instance, InstanceStatus};
use crate::port_forward::{self, PortForward};
use crate::ssh::{ensure_ssh_keypair, read_public_key};
use crate::virtumfile;
use crate::vm::{self, cleanup_failed_run, lab_vm_id, launch_vm_inner, VmLaunchConfig};

pub fn up(file: Option<PathBuf>) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(up_async(file))
}

pub fn down(file: Option<PathBuf>, destroy: bool) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(down_async(file, destroy))
}

fn resolve_virtumfile_path(file: Option<PathBuf>) -> Result<PathBuf> {
    let path = file.unwrap_or_else(|| PathBuf::from(virtumfile::DEFAULT_VIRTUMFILE));
    if path.is_file() {
        return Ok(path);
    }
    bail!(
        "Virtumfile introuvable: {}. \
         Lancez la commande depuis le répertoire du lab ou utilisez `-f /chemin/Virtumfile`.",
        path.display()
    );
}

async fn up_async(file: Option<PathBuf>) -> Result<()> {
    let path = resolve_virtumfile_path(file)?;
    let lab = virtumfile::load(&path)?;
    eprintln!(
        "Lab '{}' ({} machine(s)) depuis {}…",
        lab.name,
        lab.machines.len(),
        path.display()
    );

    let paths = config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;

    let lab_net = lab_network::ensure_lab_network(&lab.name)?;

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
        let vm_id = lab_vm_id(&lab.name, machine_name);
        if metadata::find_by_lab_machine(&metadata, &lab.name, machine_name).is_some() {
            continue;
        }
        port_forward::ensure_ports_available(forwards)?;
        port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), forwards)?;
    }

    for machine_name in lab.machines.keys() {
        let vm_id = lab_vm_id(&lab.name, machine_name);
        if metadata::find_by_lab_machine(&metadata, &lab.name, machine_name).is_some() {
            continue;
        }
        if domain_exists(&vm_id) {
            eprintln!(
                "Domaine orphelin '{vm_id}' (machine '{machine_name}') : nettoyage avant déploiement…"
            );
            let instance_dir = paths.instances.join(&vm_id);
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

        if let Some(existing) = metadata::find_by_lab_machine(&metadata, &lab.name, machine) {
            port_forward::ensure_forwards_match_existing(machine_name, &spec_ports, existing)?;
            if !spec_ports.is_empty() {
                port_forward::ensure_ports_available(&spec_ports)?;
                port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), &spec_ports)?;
            }
            let instance_dir = paths.instances.join(&vm_id);
            match resume_lab_machine(existing, &vm_id, machine_name, &instance_dir, &spec_ports)
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
            port_forwards: spec_ports,
            libvirt_network: Some(lab_net.clone()),
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
            rollback_lab_vms(&paths, &created_this_run).await;
            return Err(err);
        }
        created_this_run.push(vm_id);
        ready += 1;
    }

    println!("Lab '{}' prêt ({} VM).", lab.name, ready);
    println!("  virtum ps");
    println!("  virtum shell <name>   # ex. virtum shell web");
    Ok(())
}

/// Relance une machine de lab déjà enregistrée (après `virtum down` sans `--destroy`).
async fn resume_lab_machine(
    existing: &Instance,
    vm_id: &str,
    machine_name: &str,
    instance_dir: &std::path::Path,
    spec_ports: &[PortForward],
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
                port_forward::start_instance_proxies(instance_dir, ip, spec_ports)?;
            }
            port_forward::print_forward_lines(spec_ports);
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

async fn down_async(file: Option<PathBuf>, destroy: bool) -> Result<()> {
    let path = resolve_virtumfile_path(file)?;
    let lab = virtumfile::load(&path)?;
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
        for reference in &refs {
            vm::destroy_vm_async(reference).await?;
        }
        if let Err(err) = lab_network::destroy_lab_network(&lab.name) {
            eprintln!(
                "Avertissement: impossible de supprimer le réseau du lab '{}': {err:#}",
                lab.name
            );
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
