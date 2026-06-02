//! Lab orchestration via `Virtumfile` (`virtum up` / `virtum down`).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::config::{self, VirtumPaths};
use crate::libvirt::domain_exists;
use crate::metadata;
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
        "Déploiement du lab '{}' ({} machine(s)) depuis {}…",
        lab.name,
        lab.machines.len(),
        path.display()
    );

    let paths = config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;

    for machine_name in lab.machines.keys() {
        let vm_id = lab_vm_id(&lab.name, machine_name);
        if domain_exists(&vm_id) {
            if metadata::find_by_lab_machine(&metadata, &lab.name, machine_name).is_some() {
                bail!(
                    "Le domaine libvirt '{vm_id}' existe déjà pour la machine '{machine_name}' du lab '{}'. \
                     Exécutez `virtum down -f {}` ou `virtum destroy {machine_name}`.",
                    lab.name,
                    path.display()
                );
            }
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

    let mut deployed: Vec<String> = Vec::new();
    for (machine_name, spec) in &lab.machines {
        let machine = machine_name.as_str();
        let vm_id = lab_vm_id(&lab.name, machine);
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
        };
        let hostname = config.hostname_for_vm(&vm_id);
        let instance_dir = paths.instances.join(&vm_id);

        eprintln!("--- Machine '{machine_name}' ({vm_id}) ---");
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
            rollback_lab_vms(&paths, &deployed).await;
            return Err(err);
        }
        deployed.push(vm_id);
    }

    println!("Lab '{}' déployé ({} VM).", lab.name, deployed.len());
    println!("  virtum ps");
    println!("  virtum shell <name>   # ex. virtum shell web");
    Ok(())
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
