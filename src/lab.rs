//! Lab orchestration via `Virtumfile` (`virtum up` / `virtum down`).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::config::{self, VirtumPaths};
use crate::images::{ensure_image, resolve_image};
use crate::lab_network;
use crate::libvirt::domain_exists;
use crate::metadata::{self, Instance, InstanceStatus};
use crate::mounts;
use crate::parallel::{JobReport, JobStatus};
use crate::port_forward::{self, PortBind, PortForward};
use crate::ssh::{ensure_ssh_keypair, read_public_key};
use crate::virtumfile::{self, RestartPolicy};
use crate::vm::{self, cleanup_failed_run, lab_vm_id, launch_vm_inner, VmLaunchConfig};

pub fn up(file: Option<PathBuf>, insecure_ssh: bool, yes_lan: bool, parallel: usize) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(up_async(file, insecure_ssh, yes_lan, parallel))
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

async fn up_async(
    file: Option<PathBuf>,
    insecure_ssh: bool,
    yes_lan: bool,
    parallel: usize,
) -> Result<()> {
    let path = virtumfile::resolve_virtumfile_path(file)?;
    let vf_dir = virtumfile::virtumfile_dir(&path);
    let lab = virtumfile::load(&path)?;
    crate::verr!(
        "Lab '{}' ({} machine(s)) depuis {}…",
        lab.name,
        lab.machines.len(),
        path.display()
    );

    if virtumfile::lab_requires_lan_consent(&lab)? && !yes_lan {
        confirm_lan_deploy()?;
    }
    // Ansible déclaré mais absent : on s'arrête avant de créer la moindre VM.
    if let Some(ref spec) = lab.ansible {
        crate::ansible::require_tools(spec)?;
    }

    let paths = config::ensure_layout().await?;
    let metadata = metadata::load_metadata(&paths.metadata).await?;

    let lab_net = if virtumfile::lab_needs_isolated_network(&lab) {
        // Chevauchement avec un autre réseau (lab, `default`, interface de l'hôte) :
        // message clair et sous-réseau libre proposé, plutôt que l'erreur de libvirt.
        let conflicts = lab_network::subnet_conflicts(&lab)?;
        if let Some(msg) = conflicts.first() {
            bail!("{msg}");
        }
        let no_vm = metadata::instances_for_lab(&metadata, &lab.name).is_empty();
        Some(lab_network::ensure_lab_network(
            &lab.name,
            lab_network::lab_subnet(&lab),
            no_vm,
        )?)
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
        let domain = domain_exists(&vm_id);
        if domain && !orphan_instance {
            // Domaine absent de notre metadata et sans disque dans notre état : il peut
            // appartenir à un autre utilisateur du même libvirt (même nom de lab).
            match crate::ownership::domain_owner(&vm_id) {
                crate::ownership::Owner::Us => {}
                other => bail!(
                    "Le domaine libvirt {vm_id} ('{}' / '{machine_name}') existe déjà mais \
                     n'appartient pas à cet état Virtum{}. Refus de le supprimer : renommez le lab \
                     (`name:`) ou, s'il est bien à vous, `virtum destroy {vm_id}`.",
                    lab.name,
                    match other {
                        crate::ownership::Owner::Other(id) => format!(" (propriétaire : {id})"),
                        _ => String::new(),
                    }
                ),
            }
        }
        if domain || orphan_instance {
            crate::verr!("Nettoyage avant déploiement de '{machine_name}' ({vm_id})…");
            cleanup_failed_run(&vm_id, &instance_dir).await;
        }
        metadata::ensure_lab_machine_available(&metadata, &lab.name, machine_name)?;
    }

    let owners = lab_network::lab_instance_macs(
        &paths.instances,
        &metadata::instances_for_lab(&metadata, &lab.name),
    );
    if let Some(net) = lab_net.as_deref() {
        let (conflicts, ip_warnings) = declared_ip_checks(&lab, net, &owners);
        for w in &ip_warnings {
            crate::verr!("Avertissement: {w}");
        }
        if !conflicts.is_empty() {
            for c in &conflicts {
                crate::verr!("✗ {c}");
            }
            bail!("Adresse(s) `ip:` déjà utilisée(s) sur le réseau du lab.");
        }
    }
    let mut taken_macs: Vec<String> = owners.iter().map(|(_, mac)| mac.clone()).collect();

    let keys = ensure_ssh_keypair(&paths.keys)?;
    let public_key = read_public_key(&keys.public_key)?;

    // --- Préparation (séquentielle) : tout ce qui est partagé est décidé ici, dans
    // l'ordre du Virtumfile (MAC, réservations DHCP, images), avant tout lancement.
    let mut jobs: Vec<UpJob> = Vec::new();
    for (machine_name, spec) in &lab.machines {
        let machine = machine_name.as_str();
        let vm_id = lab_vm_id(&lab.name, machine);
        let instance_dir = paths.instances.join(&vm_id);
        let spec_ports = virtumfile::machine_port_forwards(spec)?;
        let port_bind = virtumfile::effective_port_bind(&lab, machine);

        if let Some(existing) = metadata::find_by_lab_machine(&metadata, &lab.name, machine) {
            if let Ok(diff) =
                crate::lab_diff::diff_machine(&lab, machine_name, spec, existing, &instance_dir)
            {
                if diff.changes.iter().any(|c| c.needs_apply()) {
                    bail!(
                        "Le Virtumfile de la machine '{machine_name}' ne correspond plus à l'instance déployée. \
                         Appliquez les changements avec : virtum apply -f {}\n\
                         (ou `virtum down -f … --destroy` puis `virtum up` pour recréer).",
                        path.display()
                    );
                }
            }
            if !spec_ports.is_empty() {
                // Les ports déjà attribués à cette machine (même bind) sont tenus par son
                // propre socat quand elle tourne : seuls les autres doivent être libres.
                let to_check = ports_not_owned(&spec_ports, port_bind, existing);
                port_forward::ensure_ports_available(&to_check, port_bind)?;
                port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), &spec_ports)?;
            }
            if let (Some(ip), Some(net)) = (existing.static_ip.as_deref(), lab_net.as_deref()) {
                // Réseau recréé à la main : la réservation est reposée (idempotent).
                if let Some((_, mac)) = owners.iter().find(|(m, _)| m == machine) {
                    lab_network::ensure_dhcp_reservation(net, mac, &existing.hostname, ip)?;
                }
            }
            jobs.push(UpJob::Resume {
                machine: machine.to_string(),
                vm_id,
                instance_dir,
                existing: Box::new(existing.clone()),
                spec_ports,
                port_bind,
            });
            continue;
        }

        let net_cfg = virtumfile::resolve_machine_network(&lab, machine, lab_net.as_deref())?;
        if net_cfg.mode == virtumfile::NetworkMode::Bridge {
            crate::verr!(
                "  '{machine_name}' : réseau bridge (LAN) sur {}",
                net_cfg.bridge_host_interface.as_deref().unwrap_or("?")
            );
        } else if net_cfg.bridge_host_interface.is_some() {
            crate::verr!(
                "  '{machine_name}' : réseau lab {} + bridge (LAN)",
                net_cfg.libvirt_network.as_deref().unwrap_or("?")
            );
        }
        // MAC stable pour la carte lab : même MAC après destroy + up, réservation possible
        // avant le premier démarrage.
        let nic_mac = net_cfg.libvirt_network.as_ref().map(|_| {
            let mac = lab_network::deterministic_mac(&vm_id, &taken_macs);
            taken_macs.push(mac.clone());
            mac
        });
        let static_ip = virtumfile::machine_static_ip(spec).map(|ip| ip.to_string());
        if let (Some(ip), Some(mac), Some(net)) = (
            static_ip.as_deref(),
            nic_mac.as_deref(),
            net_cfg.libvirt_network.as_deref(),
        ) {
            let hostname = format!("{}-{machine}", lab.name);
            lab_network::ensure_dhcp_reservation(net, mac, &hostname, ip)?;
            crate::verr!("  '{machine_name}' : adresse déclarée {ip} (réservation DHCP)");
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
            ssh_port: virtumfile::effective_ssh_port(&lab, machine),
            restart: virtumfile::effective_restart(&lab, machine),
            nic_mac,
            static_ip,
        };
        let hostname = config.hostname_for_vm(&vm_id);
        jobs.push(UpJob::Create {
            machine: machine.to_string(),
            vm_id,
            instance_dir,
            hostname,
            config: Box::new(config),
        });
    }

    // Images de base : une fois par OS, avant les créations parallèles (sinon plusieurs
    // fils téléchargeraient la même image).
    let mut oses: Vec<&str> = jobs
        .iter()
        .filter_map(|j| match j {
            UpJob::Create { config, .. } => Some(config.os.as_str()),
            UpJob::Resume { .. } => None,
        })
        .collect();
    oses.sort_unstable();
    oses.dedup();
    for os in oses {
        let image = resolve_image(os, &paths.images)?;
        ensure_image(&image).await?;
    }

    // --- Exécution (parallèle, au plus `parallel` machines à la fois).
    let workers = parallel.clamp(1, jobs.len().max(1));
    let concurrent = workers > 1;
    if concurrent {
        crate::verr!(
            "Création / reprise de {} machine(s), {} à la fois…",
            jobs.len(),
            workers
        );
    }
    let labels: Vec<String> = jobs.iter().map(|j| j.machine().to_string()).collect();
    let started_all = std::time::Instant::now();
    let reports = crate::parallel::run_jobs(&labels, workers, concurrent, |i| {
        let job = &jobs[i];
        let (paths, lab, public_key) = (&paths, &lab, &public_key);
        Box::pin(async move { run_up_job(job, paths, lab, public_key, concurrent).await })
    });
    let total = started_all.elapsed();

    // --- Récapitulatif, puis retour arrière si une machine a échoué.
    let meta = metadata::load_metadata(&paths.metadata).await?;
    print_up_summary(&jobs, &reports, &meta, total);
    let created_this_run: Vec<String> = jobs
        .iter()
        .zip(&reports)
        .filter(|(job, report)| job.is_create() && report.status == JobStatus::Done)
        .map(|(job, _)| job.vm_id().to_string())
        .collect();
    if let Some((job, cause)) = first_failure(&jobs, &reports) {
        let to_rollback = vms_to_rollback(&jobs, &reports);
        crate::verr!("Échec sur la machine '{}': {cause}", job.machine());
        rollback_lab_vms(&paths, &to_rollback).await;
        bail!("Échec sur la machine '{}': {cause}", job.machine());
    }
    let ready = reports
        .iter()
        .filter(|r| r.status == JobStatus::Done)
        .count();

    crate::vout!("Lab '{}' prêt ({} VM).", lab.name, ready);
    let leftover: Vec<String> = {
        let meta = metadata::load_metadata(&paths.metadata).await?;
        crate::lab_diff::removed_instances(&lab, &meta)
            .iter()
            .map(|i| i.machine_name.clone().unwrap_or_else(|| i.id.clone()))
            .collect()
    };
    if !leftover.is_empty() {
        crate::verr!(
            "  Machines hors du Virtumfile, toujours présentes : {}. Voir `virtum plan` / `virtum apply --prune -f {}`.",
            leftover.join(", "),
            path.display()
        );
    }
    provision_after_up(
        &lab,
        &path,
        &paths,
        !created_this_run.is_empty(),
        insecure_ssh,
    )
    .await?;
    for (machine_name, spec) in &lab.machines {
        let autostarts = virtumfile::effective_restart(&lab, machine_name)
            .is_some_and(|r| r != RestartPolicy::No);
        if autostarts && !spec.ports.is_empty() {
            crate::verr!(
                "  Note '{machine_name}' : après un redémarrage de l'hôte, la VM repart seule \
                 mais les `ports:` (socat) ne reviennent qu'avec `virtum up`."
            );
        }
    }
    crate::vout!("  virtum ps");
    crate::vout!("  virtum shell <name>   # ex. virtum shell web");
    crate::vout!(
        "  virtum plan -f {}   # virtum apply si le Virtumfile a changé",
        path.display()
    );
    if crate::firewall::lab_needs_firewall_hint(&lab) {
        crate::vout!(
            "  Accès LAN (bind: lan) : ouvrez le pare-feu hôte avec :\n    \
             virtum open -f {}",
            path.display()
        );
    }
    Ok(())
}

/// Conflits bloquants des `ip:` du Virtumfile avec les autres machines déployées du lab.
pub fn declared_ip_conflicts(
    lab: &virtumfile::Virtumfile,
    net_name: &str,
    owners: &[(String, String)],
) -> Vec<String> {
    declared_ip_checks(lab, net_name, owners).0
}

/// `(erreurs bloquantes, avertissements)` pour les `ip:` du Virtumfile :
/// adresse tenue par une autre machine vivante, ou encore louée à une VM supprimée.
pub fn declared_ip_checks(
    lab: &virtumfile::Virtumfile,
    net_name: &str,
    owners: &[(String, String)],
) -> (Vec<String>, Vec<String>) {
    let declared: Vec<lab_network::DeclaredIp<'_>> = lab
        .machines
        .iter()
        .filter_map(|(machine, spec)| {
            let ip = virtumfile::machine_static_ip(spec)?.to_string();
            let mac = owners
                .iter()
                .find(|(m, _)| m == machine)
                .map(|(_, mac)| mac.clone());
            Some(lab_network::DeclaredIp {
                machine: machine.as_str(),
                ip,
                mac,
            })
        })
        .collect();
    if declared.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let leases = lab_network::dhcp_leases(net_name);
    let hosts = lab_network::dhcp_hosts(net_name).unwrap_or_default();
    (
        lab_network::ip_conflicts(&declared, owners, &leases, &hosts),
        lab_network::stale_lease_warnings(&declared, owners, &leases),
    )
}

/// Ancien bail d'une autre MAC encore actif : dnsmasq a pu donner une autre adresse.
async fn warn_if_ip_differs(paths: &VirtumPaths, vm_id: &str, machine: &str, declared: &str) {
    let Ok(meta) = metadata::load_metadata(&paths.metadata).await else {
        return;
    };
    let got = metadata::find_instance(&meta, vm_id).and_then(|i| i.ip.clone());
    if let Some(got) = got.filter(|ip| ip != declared) {
        crate::verr!(
            "Avertissement '{machine}' : adresse obtenue {got} au lieu de {declared}. \
             Un ancien bail DHCP occupe probablement encore {declared} ; \
             il expire dans l'heure, puis `virtum stop {machine}` / `virtum start {machine}`."
        );
    }
}

/// Ports du Virtumfile qui ne sont pas déjà ceux de l'instance (même port hôte, même bind).
fn ports_not_owned(
    spec_ports: &[PortForward],
    bind: PortBind,
    existing: &Instance,
) -> Vec<PortForward> {
    let owned = |pf: &PortForward| {
        existing.port_bind == Some(bind)
            && existing
                .port_forwards
                .as_ref()
                .is_some_and(|ports| ports.iter().any(|p| p.host_port == pf.host_port))
    };
    spec_ports.iter().filter(|pf| !owned(pf)).cloned().collect()
}

/// Machine à créer ou à reprendre pendant `up` (préparée séquentiellement).
enum UpJob {
    Create {
        machine: String,
        vm_id: String,
        instance_dir: PathBuf,
        hostname: String,
        config: Box<VmLaunchConfig>,
    },
    Resume {
        machine: String,
        vm_id: String,
        instance_dir: PathBuf,
        existing: Box<Instance>,
        spec_ports: Vec<PortForward>,
        port_bind: PortBind,
    },
}

impl UpJob {
    fn machine(&self) -> &str {
        match self {
            Self::Create { machine, .. } | Self::Resume { machine, .. } => machine,
        }
    }

    fn vm_id(&self) -> &str {
        match self {
            Self::Create { vm_id, .. } | Self::Resume { vm_id, .. } => vm_id,
        }
    }

    fn is_create(&self) -> bool {
        matches!(self, Self::Create { .. })
    }
}

/// Crée ou reprend une machine (exécuté dans le fil de la machine).
async fn run_up_job(
    job: &UpJob,
    paths: &VirtumPaths,
    lab: &virtumfile::Virtumfile,
    public_key: &str,
    concurrent: bool,
) -> Result<()> {
    if !concurrent {
        crate::verr!("--- Machine '{}' ({}) ---", job.machine(), job.vm_id());
    }
    match job {
        UpJob::Create {
            machine,
            vm_id,
            instance_dir,
            hostname,
            config,
        } => {
            let mut retain = false;
            launch_vm_inner(
                config,
                paths,
                instance_dir,
                vm_id,
                hostname,
                public_key,
                &mut retain,
            )
            .await?;
            if let Some(ref declared) = config.static_ip {
                warn_if_ip_differs(paths, vm_id, machine, declared).await;
            }
            Ok(())
        }
        UpJob::Resume {
            machine,
            vm_id,
            instance_dir,
            existing,
            spec_ports,
            port_bind,
        } => {
            resume_lab_machine(
                existing,
                vm_id,
                machine,
                instance_dir,
                spec_ports,
                *port_bind,
                lab,
            )
            .await
        }
    }
}

/// Première machine en échec et sa cause.
fn first_failure<'a>(jobs: &'a [UpJob], reports: &[JobReport]) -> Option<(&'a UpJob, String)> {
    jobs.iter()
        .zip(reports)
        .find_map(|(job, r)| match &r.status {
            JobStatus::Failed(cause) => Some((job, cause.clone())),
            _ => None,
        })
}

/// Machines à défaire après un échec : toutes celles **créées** par ce `up` qui ont
/// commencé (terminées, en échec ou annulées en cours). Les reprises ne sont pas touchées.
fn vms_to_rollback(jobs: &[UpJob], reports: &[JobReport]) -> Vec<String> {
    rollback_ids(jobs.iter().map(|j| (j.is_create(), j.vm_id())), reports)
}

/// `(création ?, vm_id)` par machine → identifiants à défaire.
fn rollback_ids<'a>(
    jobs: impl Iterator<Item = (bool, &'a str)>,
    reports: &[JobReport],
) -> Vec<String> {
    jobs.zip(reports)
        .filter(|((is_create, _), report)| *is_create && report.started())
        .map(|((_, vm_id), _)| vm_id.to_string())
        .collect()
}

fn print_up_summary(
    jobs: &[UpJob],
    reports: &[JobReport],
    meta: &metadata::Metadata,
    total: std::time::Duration,
) {
    let width = jobs.iter().map(|j| j.machine().len()).max().unwrap_or(0);
    crate::verr!(
        "Récapitulatif ({} machine(s), {}) :",
        jobs.len(),
        crate::parallel::format_duration(total)
    );
    for (job, report) in jobs.iter().zip(reports) {
        let state = match (&report.status, job.is_create()) {
            (JobStatus::Done, true) => "créée",
            (JobStatus::Done, false) => "reprise",
            (JobStatus::Failed(_), _) => "échec",
            (JobStatus::Cancelled, _) => "annulée",
            (JobStatus::NotStarted, _) => "non lancée",
        };
        let ip = metadata::find_instance(meta, job.vm_id())
            .and_then(|i| i.ip.clone())
            .filter(|_| report.status == JobStatus::Done)
            .unwrap_or_else(|| "-".to_string());
        crate::verr!(
            "  {:<width$}  {state:<10}  {ip:<15}  {}",
            job.machine(),
            crate::parallel::format_duration(report.elapsed)
        );
    }
}

/// Joue le playbook `ansible:` à la fin de `up` si le lab n'a jamais été provisionné avec
/// succès ou si des machines viennent d'être créées ; puis prend l'instantané de lab.
async fn provision_after_up(
    lab: &virtumfile::Virtumfile,
    path: &std::path::Path,
    paths: &VirtumPaths,
    created_machines: bool,
    insecure_ssh: bool,
) -> Result<()> {
    let Some(ref spec) = lab.ansible else {
        return Ok(());
    };
    let previous = crate::ansible::load_state(paths, &lab.name)?;
    let Some(reason) = crate::ansible::provision_reason(previous.as_ref(), created_machines) else {
        let when = previous
            .as_ref()
            .and_then(|s| s.provisioned_at.clone())
            .unwrap_or_default();
        crate::vout!(
            "Lab déjà provisionné ({when}) — `virtum provision -f {}` pour rejouer le playbook.",
            path.display()
        );
        return Ok(());
    };
    crate::verr!("--- Provisionnement Ansible ({reason}) ---");
    match crate::ansible::run_playbook(lab, path, paths, &[], insecure_ssh).await? {
        crate::ansible::RunOutcome::Failed(code) => bail!(
            "Provisionnement en échec (ansible-playbook, code {code}) : VM conservées pour le diagnostic. \
             Corrigez puis relancez `virtum provision -f {p}` (ou `virtum up -f {p}`).",
            p = path.display()
        ),
        crate::ansible::RunOutcome::Success => crate::vout!("Provisionnement Ansible réussi."),
    }
    if let Some(name) = spec.snapshot_name() {
        take_provision_snapshot(lab, path, paths, &name).await?;
    }
    Ok(())
}

/// Instantané de lab après provisionnement ; jamais d'écrasement, échec non bloquant.
async fn take_provision_snapshot(
    lab: &virtumfile::Virtumfile,
    path: &std::path::Path,
    paths: &VirtumPaths,
    name: &str,
) -> Result<()> {
    let meta = metadata::load_metadata(&paths.metadata).await?;
    let mut with = Vec::new();
    let mut without = Vec::new();
    for inst in metadata::instances_for_lab(&meta, &lab.name) {
        let disk = paths.instances.join(&inst.id).join("disk.qcow2");
        let label = inst.machine_name.clone().unwrap_or_else(|| inst.id.clone());
        let exists = crate::snapshot::list_disk_snapshots(&disk)
            .map(|snaps| snaps.iter().any(|s| s.name == name))
            .unwrap_or(false);
        if exists {
            with.push(label);
        } else {
            without.push(label);
        }
    }
    if !with.is_empty() {
        let missing = if without.is_empty() {
            String::new()
        } else {
            format!(
                " Sans cet instantané : {} — `virtum snapshot restore {name}` sera refusé tant qu'il n'est pas repris.",
                without.join(", ")
            )
        };
        crate::verr!(
            "Avertissement: l'instantané '{name}' existe déjà (non écrasé).{missing} Pour le reprendre : \
             `virtum snapshot rm {name} -f {p}` puis `virtum snapshot create {name} -f {p}`.",
            p = path.display()
        );
        return Ok(());
    }
    crate::verr!("--- Instantané '{name}' du lab provisionné ---");
    match crate::snapshot::create_async(name, &[], Some(path.to_path_buf()), false).await {
        Ok(()) => {
            let mut state = crate::ansible::load_state(paths, &lab.name)?.unwrap_or_default();
            state.snapshot = Some(name.to_string());
            crate::ansible::save_state(paths, &lab.name, &state)?;
            crate::vout!(
                "Retour à ce lab provisionné : virtum snapshot restore {name} -f {}",
                path.display()
            );
        }
        Err(err) => crate::verr!(
            "Avertissement: instantané '{name}' non pris ({err:#}). Provisionnement conservé ; \
             relancez `virtum snapshot create {name} -f {}`.",
            path.display()
        ),
    }
    Ok(())
}

pub(crate) fn confirm_lan_deploy() -> Result<()> {
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
        crate::verr!("\nDéploiement annulé.");
        std::process::exit(0);
    }
    let answer = line.trim();
    if answer == "y" || answer == "Y" {
        return Ok(());
    }
    crate::verr!("Déploiement annulé.");
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
    lab: &virtumfile::Virtumfile,
) -> Result<()> {
    let ssh_port = virtumfile::effective_ssh_port(lab, machine_name);
    let restart = virtumfile::effective_restart(lab, machine_name);
    if existing.id != vm_id {
        bail!(
            "Incohérence lab/machine '{machine_name}' : metadata {} vs domaine attendu {vm_id}. \
             Utilisez `virtum destroy {machine_name}` puis `virtum up`.",
            existing.id
        );
    }

    let reference = existing.name.clone().unwrap_or_else(|| existing.id.clone());

    let paths = config::ensure_layout().await?;
    // `ssh_port` du Virtumfile pris en compte dès `up` (pas besoin d'`apply`).
    let mut existing = existing.clone();
    let stored_port = vm::stored_ssh_port(ssh_port);
    if existing.ssh_port != stored_port {
        crate::verr!("  Port SSH de '{machine_name}' : {ssh_port}");
        existing.ssh_port = stored_port;
        metadata::upsert_instance(&paths.metadata, existing.clone()).await?;
    }
    if existing.restart != restart {
        crate::verr!(
            "  Démarrage au boot de '{machine_name}' : restart: {}",
            restart.map_or("(non géré)", RestartPolicy::label)
        );
        // `restart` retiré du Virtumfile : on désactive l'autostart que Virtum avait posé.
        if restart.is_none() && domain_exists(vm_id) {
            crate::libvirt::set_domain_autostart(vm_id, false)?;
        }
        existing.restart = restart;
        metadata::upsert_instance(&paths.metadata, existing.clone()).await?;
    }

    if domain_exists(vm_id) {
        let live = vm::refresh_instance_status(&paths.metadata, existing.clone()).await?;
        if live.status == InstanceStatus::Running {
            vm::sync_autostart(&existing, true)?;
            let ip = existing.ip.as_deref().unwrap_or("-");
            crate::verr!("Machine '{machine_name}' déjà en cours d'exécution (IP: {ip}).");
            if !spec_ports.is_empty() && ip != "-" {
                port_forward::start_instance_proxies(instance_dir, ip, spec_ports, port_bind)?;
            }
            port_forward::print_forward_lines(spec_ports, port_bind);
            crate::vout!("  SSH:      virtum shell {reference}");
            return Ok(());
        }
    }

    crate::verr!("Relance de la machine '{machine_name}'…");
    vm::start_vm_async(&reference).await
}

async fn rollback_lab_vms(paths: &VirtumPaths, vm_ids: &[String]) {
    if vm_ids.is_empty() {
        return;
    }
    crate::verr!("Nettoyage des machines déjà créées dans ce lab…");
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
        crate::verr!("Aucune VM active pour le lab '{}'.", lab.name);
        return Ok(());
    }

    let refs: Vec<String> = instances
        .iter()
        .map(|i| i.name.clone().unwrap_or_else(|| i.id.clone()))
        .collect();

    if destroy {
        crate::verr!("Suppression de {} VM du lab '{}'…", refs.len(), lab.name);
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
                crate::verr!(
                    "Dossiers mounts hôte supprimés ({n}) sous {}.",
                    vf_dir.display()
                );
            }
        }
        if virtumfile::lab_needs_isolated_network(&lab) {
            if let Err(err) = lab_network::destroy_lab_network(&lab.name) {
                crate::verr!(
                    "Avertissement: impossible de supprimer le réseau du lab '{}': {err:#}",
                    lab.name
                );
            }
        }
        crate::ansible::remove_state(&paths, &lab.name);
        crate::vout!("Lab '{}' supprimé.", lab.name);
    } else {
        crate::verr!("Arrêt de {} VM du lab '{}'…", refs.len(), lab.name);
        for reference in &refs {
            vm::stop_vm_async(reference).await?;
        }
        crate::vout!("Lab '{}' arrêté.", lab.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_only_started_creations() {
        let report = |status| JobReport {
            label: String::new(),
            status,
            elapsed: std::time::Duration::ZERO,
        };
        let jobs = [
            (true, "vm-a"),  // créée, terminée
            (false, "vm-b"), // reprise en échec : existait avant, intouchée
            (true, "vm-c"),  // créée, annulée en cours
            (true, "vm-d"),  // jamais lancée : rien à défaire
        ];
        let reports = [
            report(JobStatus::Done),
            report(JobStatus::Failed("boom".into())),
            report(JobStatus::Cancelled),
            report(JobStatus::NotStarted),
        ];
        assert_eq!(
            rollback_ids(jobs.iter().copied(), &reports),
            vec!["vm-a".to_string(), "vm-c".to_string()]
        );
    }

    #[test]
    fn ports_already_owned_are_not_rechecked() {
        let pf = |h: u16, g: u16| PortForward {
            host_port: h,
            guest_port: g,
        };
        let mut inst: Instance = serde_json::from_str(
            r#"{"id":"vm-cp","name":"cp","os":"ubuntu-24.04","status":"running",
                "user":"virtum","hostname":"h","created_at":"2026-11-20T09:00:00Z"}"#,
        )
        .unwrap();
        inst.port_forwards = Some(vec![pf(6443, 6443)]);
        inst.port_bind = Some(PortBind::Local);
        let spec = vec![pf(6443, 6443), pf(30080, 30080)];
        assert_eq!(
            ports_not_owned(&spec, PortBind::Local, &inst),
            vec![pf(30080, 30080)]
        );
        // Bind différent : le port est rouvert ailleurs, il faut le vérifier.
        assert_eq!(ports_not_owned(&spec, PortBind::Lan, &inst), spec);
        inst.port_forwards = None;
        assert_eq!(ports_not_owned(&spec, PortBind::Local, &inst), spec);
    }
}
