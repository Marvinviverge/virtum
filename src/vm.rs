//! VM lifecycle orchestration (steps 15–20).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::fs;
use tokio::time::sleep;
use uuid::Uuid;

use crate::cli::RunArgs;
use crate::cloud_init::{build_cloud_init_iso, write_meta_data, write_user_data};
use crate::config::{self, VirtumPaths};
use crate::disk::{
    create_cow_disk, format_virtual_size_label, image_virtual_size_bytes, parse_disk_size,
    plan_disk_size,
};
use crate::images::{ensure_image, resolve_image};
use crate::lab_network;
use crate::libvirt::{
    self, define_domain, destroy_domain, domain_exists, domain_is_active, domain_state,
    shutdown_domain, start_domain, undefine_domain, write_domain_xml, DomainSpec,
};
use crate::libvirt_access;
use crate::metadata::{self, Instance, InstanceStatus};
use crate::port_forward::{self, PortForward};
use crate::ssh::{ensure_ssh_keypair, read_public_key, run_provision_commands, ssh_auth_probe};

const IP_POLL_INTERVAL: Duration = Duration::from_secs(2);
const IP_TIMEOUT: Duration = Duration::from_secs(60);
const IP_BOOT_GRACE: Duration = Duration::from_secs(5);
const SSH_POLL_INTERVAL: Duration = Duration::from_secs(2);
const SSH_TIMEOUT: Duration = Duration::from_secs(120);
const SSH_TIMEOUT_GROW_ROOT: Duration = Duration::from_secs(300);
const SSH_TIMEOUT_PROVISION: Duration = Duration::from_secs(600);
const SSH_BOOT_GRACE: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_PORT: u16 = 22;

/// Paramètres de création d'une VM (CLI `run` ou Virtumfile).
#[derive(Debug, Clone)]
pub struct VmLaunchConfig {
    pub os: String,
    pub ram: u32,
    pub cpu: u32,
    pub user: String,
    pub name: Option<String>,
    pub disk_size: Option<String>,
    pub provision: Vec<String>,
    pub lab_name: Option<String>,
    pub machine_name: Option<String>,
    pub port_forwards: Vec<PortForward>,
    /// Réseau libvirt (`virtum-lab-…` pour un lab, sinon `default` via `None`).
    pub libvirt_network: Option<String>,
}

impl VmLaunchConfig {
    pub fn from_run_args(args: &RunArgs) -> Self {
        Self {
            os: args.os.clone(),
            ram: args.ram,
            cpu: args.cpu,
            user: args.user.clone(),
            name: args.name.clone(),
            disk_size: args.disk_size.clone(),
            provision: Vec::new(),
            lab_name: None,
            machine_name: None,
            port_forwards: Vec::new(),
            libvirt_network: None,
        }
    }

    pub fn hostname_for_vm(&self, vm_id: &str) -> String {
        if let Some(ref lab) = self.lab_name {
            if let Some(ref machine) = self.machine_name {
                return format!("{lab}-{machine}");
            }
        }
        self.name.clone().unwrap_or_else(|| vm_id.to_string())
    }
}

/// Id libvirt stable pour une machine de lab (`virtum up`).
pub fn lab_vm_id(lab_name: &str, machine_name: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(lab_name.as_bytes());
    hasher.update(b":");
    hasher.update(machine_name.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("vm-{}", &digest[..12])
}

pub fn run_vm(args: RunArgs) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(run_vm_async(args))
}

pub async fn run_vm_async(args: RunArgs) -> Result<()> {
    let paths = config::ensure_layout().await?;
    if let Some(ref name) = args.name {
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        metadata::ensure_name_available(&metadata, name)?;
    }
    let keys = ensure_ssh_keypair(&paths.keys)?;
    let public_key = read_public_key(&keys.public_key)?;

    let vm_id = generate_vm_id();
    if domain_exists(&vm_id) {
        bail!(
            "Collision de nom libvirt improbable mais détectée pour '{vm_id}'. Relancez virtum run."
        );
    }

    let config = VmLaunchConfig::from_run_args(&args);
    let hostname = args
        .hostname
        .clone()
        .unwrap_or_else(|| config.hostname_for_vm(&vm_id));
    let instance_dir = paths.instances.join(&vm_id);

    let mut retain_vm_on_error = false;
    let run_result = tokio::select! {
        res = launch_vm_inner(
            &config,
            &paths,
            &instance_dir,
            &vm_id,
            &hostname,
            &public_key,
            &mut retain_vm_on_error,
        ) => res,
        _ = tokio::signal::ctrl_c() => {
            if !retain_vm_on_error {
                cleanup_failed_run(&vm_id, &instance_dir).await;
            }
            bail!(
                "Interrompu (Ctrl+C).{}",
                if retain_vm_on_error {
                    format!(
                        " La VM '{vm_id}' peut encore tourner ; utilisez `virtum destroy {vm_id}`."
                    )
                } else {
                    String::new()
                }
            );
        }
    };

    if let Err(err) = run_result {
        if retain_vm_on_error {
            eprintln!(
                "Avertissement: la VM '{vm_id}' est démarrée mais l'opération a échoué.\n\
                 Utilisez `virtum destroy {vm_id}` pour libérer les ressources."
            );
        } else {
            cleanup_failed_run(&vm_id, &instance_dir).await;
        }
        return Err(err);
    }

    Ok(())
}

pub async fn launch_vm_inner(
    config: &VmLaunchConfig,
    paths: &VirtumPaths,
    instance_dir: &Path,
    vm_id: &str,
    hostname: &str,
    public_key: &str,
    retain_vm_on_error: &mut bool,
) -> Result<()> {
    fs::create_dir_all(instance_dir)
        .await
        .with_context(|| format!("Impossible de créer {}", instance_dir.display()))?;

    let image = resolve_image(&config.os, &paths.images)?;
    let base_image = ensure_image(&image).await?;

    let disk_path = instance_dir.join("disk.qcow2");
    let grow_root = if let Some(ref size_str) = config.disk_size {
        let size = parse_disk_size(size_str)?;
        let grow = plan_disk_size(&base_image, &size)?;
        create_cow_disk(&base_image, &disk_path, Some(&size))?;
        grow
    } else {
        create_cow_disk(&base_image, &disk_path, None)?;
        false
    };

    write_user_data(instance_dir, &config.user, public_key, grow_root)?;

    let disk_size_label = if let Some(ref size_str) = config.disk_size {
        size_str.clone()
    } else {
        format_virtual_size_label(image_virtual_size_bytes(&disk_path)?)
    };

    write_meta_data(instance_dir, vm_id, hostname)?;
    build_cloud_init_iso(instance_dir)?;
    let iso_path = instance_dir.join("cloud-init.iso");

    libvirt_access::prepare_storage_for_system_qemu(
        paths,
        &base_image,
        instance_dir,
        &disk_path,
        &iso_path,
    )?;

    if !config.port_forwards.is_empty() {
        port_forward::ensure_ports_available(&config.port_forwards)?;
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        port_forward::ensure_ports_not_in_metadata(&metadata, Some(vm_id), &config.port_forwards)?;
    }

    let libvirt_network = config
        .libvirt_network
        .as_deref()
        .unwrap_or(libvirt::LIBVIRT_DEFAULT_NETWORK);
    let spec = DomainSpec {
        vm_id,
        ram_mb: config.ram,
        vcpus: config.cpu,
        disk_path: &disk_path,
        iso_path: &iso_path,
        network: libvirt_network,
    };
    let xml_path = write_domain_xml(instance_dir, &spec)?;

    define_domain(&xml_path).with_context(|| {
        format!(
            "virsh define a échoué pour {vm_id}. \
             Vérifiez libvirt avec virtum doctor."
        )
    })?;

    start_domain(vm_id).with_context(|| {
        format!(
            "virsh start a échoué pour {vm_id}.\n{}",
            libvirt_access::permission_denied_hint(paths)
        )
    })?;

    let state = domain_state(vm_id)?;
    if state != "running" {
        bail!("La VM {vm_id} n'est pas en état running (état actuel: {state}).");
    }

    eprintln!(
        "Attente de l'adresse IP (réseau {}, max {} s)…",
        libvirt_network,
        IP_TIMEOUT.as_secs()
    );
    let ip = wait_for_ip(vm_id, hostname, libvirt_network, IP_TIMEOUT).await?;
    if !is_usable_ssh_ip(&ip) {
        bail!(
            "Adresse IP invalide pour '{vm_id}': {ip}. \
             Attendu une IP du réseau libvirt (ex. 192.168.122.x)."
        );
    }

    let ssh_timeout = if grow_root {
        SSH_TIMEOUT_GROW_ROOT
    } else {
        SSH_TIMEOUT
    };
    if grow_root {
        eprintln!(
            "Attente de SSH sur {ip} (cloud-init + agrandissement disque, max {} s)…",
            ssh_timeout.as_secs()
        );
    } else {
        eprintln!(
            "Attente de SSH sur {ip} (cloud-init, max {} s)…",
            ssh_timeout.as_secs()
        );
    }
    let keys = ensure_ssh_keypair(&paths.keys)?;
    let private_key = keys.private_key.display();
    wait_for_ssh_login(&keys.private_key, &config.user, &ip, SSH_PORT, ssh_timeout)
        .await
        .with_context(|| {
            format!(
                "Instance : {}/{}\n\
                 user-data : {}/user-data\n\
                 Test manuel : ssh -i {private_key} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
                 {user}@{ip}",
                paths.root.display(),
                instance_dir.file_name().unwrap_or_default().to_string_lossy(),
                instance_dir.display(),
                user = config.user,
            )
        })?;
    *retain_vm_on_error = true;

    if !config.provision.is_empty() {
        let mut post_boot: Vec<String> = Vec::new();
        if grow_root {
            post_boot.push(
                "DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
                 apt-get install -y -qq qemu-guest-agent && \
                 systemctl enable --now qemu-guest-agent"
                    .to_string(),
            );
        }
        post_boot.extend(config.provision.iter().cloned());
        eprintln!(
            "Provision post-SSH ({} commande(s), max {} s)…",
            post_boot.len(),
            SSH_TIMEOUT_PROVISION.as_secs()
        );
        run_provision_commands(
            &keys.private_key,
            &config.user,
            &ip,
            SSH_PORT,
            &post_boot,
            SSH_TIMEOUT_PROVISION,
        )?;
    } else if grow_root {
        eprintln!("Installation qemu-guest-agent post-SSH…");
        run_provision_commands(
            &keys.private_key,
            &config.user,
            &ip,
            SSH_PORT,
            &["DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
               apt-get install -y -qq qemu-guest-agent && \
               systemctl enable --now qemu-guest-agent"
                .to_string()],
            SSH_TIMEOUT_PROVISION,
        )?;
    }

    let instance = Instance {
        id: vm_id.to_string(),
        name: config.name.clone(),
        os: config.os.clone(),
        status: InstanceStatus::Running,
        ip: Some(ip.clone()),
        ssh_port: None,
        user: config.user.clone(),
        hostname: hostname.to_string(),
        disk_size: Some(disk_size_label),
        lab_name: config.lab_name.clone(),
        machine_name: config.machine_name.clone(),
        port_forwards: if config.port_forwards.is_empty() {
            None
        } else {
            Some(config.port_forwards.clone())
        },
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    metadata::upsert_instance(&paths.metadata, instance).await?;

    if !config.port_forwards.is_empty() {
        port_forward::start_instance_proxies(instance_dir, &ip, &config.port_forwards)?;
    }

    let shell_ref = config.name.as_deref().unwrap_or(vm_id);
    println!("VM démarrée: {vm_id}");
    if let Some(ref name) = config.name {
        println!("  Name:     {name}");
    }
    if let Some(ref lab) = config.lab_name {
        println!("  Lab:      {lab}");
    }
    println!("  OS:       {}", config.os);
    println!("  IP:       {ip}");
    port_forward::print_forward_lines(&config.port_forwards);
    println!("  SSH:      virtum shell {shell_ref}");
    Ok(())
}

pub async fn wait_for_ip(
    vm_id: &str,
    hostname: &str,
    libvirt_network: &str,
    timeout: Duration,
) -> Result<String> {
    let started = Instant::now();
    sleep(IP_BOOT_GRACE).await;

    loop {
        if let Some(ip) = try_discover_ip(vm_id, hostname, libvirt_network)? {
            return Ok(ip);
        }

        if started.elapsed() >= timeout {
            break;
        }
        sleep(IP_POLL_INTERVAL).await;
    }

    bail!(
        "Aucune adresse IP trouvée pour '{vm_id}' après {} secondes.\n\
         Suggestions:\n\
         - Exécutez `virtum doctor` (réseau libvirt actif)\n\
         - Vérifiez `virsh domifaddr {vm_id}` et `virsh net-dhcp-leases {libvirt_network}`\n\
         - Attendez la fin de cloud-init puis réessayez",
        timeout.as_secs()
    );
}

fn try_discover_ip(vm_id: &str, hostname: &str, libvirt_network: &str) -> Result<Option<String>> {
    for output in libvirt::domifaddr_all_sources(vm_id) {
        if let Some(ip) = libvirt::parse_ipv4_from_domifaddr(&output) {
            if is_usable_ssh_ip(&ip) {
                return Ok(Some(ip));
            }
        }
    }

    if let Some(ip) = libvirt::guest_network_ipv4_from_agent(vm_id) {
        if is_usable_ssh_ip(&ip) {
            return Ok(Some(ip));
        }
    }

    let mac = libvirt::domiflist(vm_id)
        .ok()
        .and_then(|output| libvirt::parse_mac_from_domiflist(&output));

    if let (Some(mac), Ok(leases)) = (mac.as_ref(), libvirt::net_dhcp_leases(libvirt_network)) {
        if let Some(ip) = libvirt::parse_ipv4_from_dhcp_leases(&leases, mac) {
            if is_usable_ssh_ip(&ip) && dhcp_lease_mac_matches(&leases, &ip, mac) {
                return Ok(Some(ip));
            }
        }
    }

    // Pas de recherche par hostname : réutiliser `--name disk-test` reprendrait
    // une lease DHCP périmée et ferait attendre SSH sur la mauvaise IP.
    let _ = hostname;

    Ok(None)
}

/// Vérifie que l'IP du bail DHCP correspond bien au MAC de cette VM.
fn dhcp_lease_mac_matches(leases: &str, ip: &str, mac: &str) -> bool {
    let mac = mac.to_lowercase();
    for line in leases.lines() {
        let lower = line.to_lowercase();
        if lower.contains(&mac) && line.contains(ip) {
            return true;
        }
    }
    false
}

fn is_usable_ssh_ip(ip: &str) -> bool {
    !ip.starts_with("127.")
        && ip != libvirt::SLIRP_GUEST_IPV4
        && ip != "0.0.0.0"
        && !ip.starts_with("169.254.")
}

pub async fn wait_for_ssh_login(
    private_key: &Path,
    user: &str,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    let mut last_progress = Instant::now();
    sleep(SSH_BOOT_GRACE).await;

    loop {
        if ssh_auth_probe(private_key, user, host, port) {
            return Ok(());
        }

        if last_progress.elapsed() >= Duration::from_secs(30) {
            eprintln!(
                "  … cloud-init / SSH pas encore prêt ({:.0} s / {} s)",
                started.elapsed().as_secs_f64(),
                timeout.as_secs()
            );
            last_progress = Instant::now();
        }

        if started.elapsed() >= timeout {
            break;
        }
        sleep(SSH_POLL_INTERVAL).await;
    }

    bail!(
        "Connexion SSH refusée pour {user}@{host}:{port} après {} secondes \
         (sshd pas encore joignable ; cloud-init ou growpart peut être en cours).",
        timeout.as_secs()
    );
}

pub fn start_vm(reference: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(start_vm_async(reference))
}

pub async fn start_vm_async(reference: &str) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let mut instance = require_instance(&paths, reference).await?;
    let vm_id = instance.id.clone();

    match instance.status {
        InstanceStatus::Destroyed => {
            bail!(
                "L'instance '{vm_id}' est supprimée. Lancez `virtum run` pour créer une nouvelle VM."
            );
        }
        InstanceStatus::Creating => {
            bail!(
                "L'instance '{vm_id}' est encore en cours de création. \
                 Attendez la fin de `virtum run` ou nettoyez avec `virtum destroy {vm_id}`."
            );
        }
        InstanceStatus::Running => {
            if domain_exists(&vm_id) {
                let state = domain_state(&vm_id)?;
                if state == "running" {
                    bail!(
                        "La VM '{vm_id}' est déjà démarrée (IP: {}). Utilisez `virtum shell {vm_id}`.",
                        instance.ip.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        InstanceStatus::Stopped => {}
    }

    let instance_dir = paths.instances.join(&vm_id);
    if !instance_dir.is_dir() {
        bail!(
            "Répertoire instance introuvable: {}. \
             La VM a peut-être été supprimée manuellement ; utilisez `virtum destroy {vm_id}` puis `virtum run`.",
            instance_dir.display()
        );
    }

    let disk_path = instance_dir.join("disk.qcow2");
    let iso_path = instance_dir.join("cloud-init.iso");
    let libvirt_network = lab_network::network_for_lab(instance.lab_name.as_deref())?;
    let xml_path =
        libvirt::refresh_domain_xml(&instance_dir, &vm_id, Some(libvirt_network.as_str()))?;
    for path in [&disk_path, &iso_path, &xml_path] {
        if !path.is_file() {
            bail!(
                "Fichier manquant pour redémarrer '{vm_id}': {}. \
                 Utilisez `virtum destroy {vm_id}` puis `virtum run`.",
                path.display()
            );
        }
    }

    let image = resolve_image(&instance.os, &paths.images)?;
    let base_image = ensure_image(&image).await?;

    libvirt_access::prepare_storage_for_system_qemu(
        &paths,
        &base_image,
        &instance_dir,
        &disk_path,
        &iso_path,
    )?;

    define_domain(&xml_path).with_context(|| {
        format!(
            "virsh define a échoué pour {vm_id}. \
             Vérifiez {} et virtum doctor.",
            xml_path.display()
        )
    })?;

    start_domain(&vm_id).with_context(|| {
        format!(
            "virsh start a échoué pour {vm_id}.\n{}",
            libvirt_access::permission_denied_hint(&paths)
        )
    })?;

    let state = domain_state(&vm_id)?;
    if state != "running" {
        bail!("La VM {vm_id} n'est pas en état running (état actuel: {state}).");
    }

    eprintln!(
        "Attente de l'adresse IP (réseau {}, max {} s)…",
        libvirt_network,
        IP_TIMEOUT.as_secs()
    );
    let ip = wait_for_ip(
        &vm_id,
        &instance.hostname,
        libvirt_network.as_str(),
        IP_TIMEOUT,
    )
    .await?;
    if !is_usable_ssh_ip(&ip) {
        bail!(
            "Adresse IP invalide pour '{vm_id}': {ip}. \
             Attendu une IP du réseau libvirt (ex. 192.168.122.x)."
        );
    }

    eprintln!("Attente de SSH sur {ip} (max {} s)…", SSH_TIMEOUT.as_secs());
    let keys = ensure_ssh_keypair(&paths.keys)?;
    wait_for_ssh_login(
        &keys.private_key,
        &instance.user,
        &ip,
        instance.ssh_port.unwrap_or(SSH_PORT),
        SSH_TIMEOUT,
    )
    .await?;

    let shell_ref = instance.name.clone().unwrap_or_else(|| vm_id.clone());
    let forwards_snapshot = instance.port_forwards.clone();
    instance.status = InstanceStatus::Running;
    instance.ip = Some(ip.clone());
    metadata::upsert_instance(&paths.metadata, instance).await?;

    if let Some(ref forwards) = forwards_snapshot {
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), forwards)?;
        port_forward::start_instance_proxies(&instance_dir, &ip, forwards)?;
    }

    println!("VM démarrée: {vm_id}");
    println!("  IP:       {ip}");
    if let Some(ref forwards) = forwards_snapshot {
        port_forward::print_forward_lines(forwards);
    }
    println!("  SSH:      virtum shell {shell_ref}");
    Ok(())
}

pub fn stop_vm(reference: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(stop_vm_async(reference))
}

pub async fn stop_vm_async(reference: &str) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let mut instance = require_instance(&paths, reference).await?;
    let vm_id = instance.id.clone();

    if domain_exists(&vm_id) {
        shutdown_domain(&vm_id).with_context(|| format!("virsh shutdown a échoué pour {vm_id}"))?;
        if !wait_for_domain_inactive(&vm_id, STOP_TIMEOUT).await {
            eprintln!(
                "La VM '{vm_id}' ne s'est pas arrêtée en {} s ; arrêt forcé (virsh destroy).",
                STOP_TIMEOUT.as_secs()
            );
            destroy_domain(&vm_id)
                .with_context(|| format!("virsh destroy a échoué pour {vm_id}"))?;
        }
    } else {
        eprintln!(
            "Avertissement: le domaine libvirt '{vm_id}' est introuvable ; \
             mise à jour du statut metadata uniquement."
        );
    }

    let instance_dir = paths.instances.join(&vm_id);
    port_forward::stop_instance_proxies(&instance_dir);

    instance.status = InstanceStatus::Stopped;
    metadata::upsert_instance(&paths.metadata, instance).await?;

    println!("VM arrêtée: {vm_id}");
    Ok(())
}

pub fn destroy_vm(reference: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(destroy_vm_async(reference))
}

pub async fn destroy_vm_async(reference: &str) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let (vm_id, orphan) = resolve_destroy_target(&paths, reference).await?;
    let instance_dir = paths.instances.join(&vm_id);

    if orphan {
        eprintln!("Domaine Virtum orphelin (libvirt sans entrée metadata.json) : {vm_id}");
    }

    port_forward::stop_instance_proxies(&instance_dir);

    if domain_exists(&vm_id) {
        if domain_is_active(&domain_state(&vm_id)?) {
            destroy_domain(&vm_id)
                .with_context(|| format!("virsh destroy a échoué pour {vm_id}"))?;
        }
        undefine_domain(&vm_id).with_context(|| format!("virsh undefine a échoué pour {vm_id}"))?;
    } else {
        eprintln!("Avertissement: le domaine libvirt '{vm_id}' est déjà absent.");
    }

    if instance_dir.exists() {
        fs::remove_dir_all(&instance_dir)
            .await
            .with_context(|| format!("Impossible de supprimer {}", instance_dir.display()))?;
    }

    let metadata = metadata::load_metadata(&paths.metadata).await?;
    if metadata::find_instance(&metadata, &vm_id).is_some() {
        metadata::remove_instance(&paths.metadata, &vm_id).await?;
    }

    println!("VM supprimée: {vm_id}");
    Ok(())
}

/// Résout la cible de `destroy` : metadata (id ou name) ou domaine libvirt orphelin `vm-…`.
async fn resolve_destroy_target(paths: &VirtumPaths, reference: &str) -> Result<(String, bool)> {
    let reference = reference.trim();
    let metadata = metadata::load_metadata(&paths.metadata).await?;

    if let Ok(instance) = metadata::resolve_instance_ref(&metadata, reference) {
        return Ok((instance.id.clone(), false));
    }

    if is_virtum_vm_id(reference) && domain_exists(reference) {
        return Ok((reference.to_string(), true));
    }

    if is_virtum_vm_id(reference) {
        let instance_dir = paths.instances.join(reference);
        if instance_dir.is_dir() {
            return Ok((reference.to_string(), true));
        }
    }

    bail!(
        "Instance '{reference}' introuvable. Utilisez `virtum ps -a` pour lister les VM (id ou name), \
         ou `virtum destroy vm-…` si le domaine apparaît dans `virsh list --all`."
    );
}

/// Id libvirt généré par Virtum (`vm-` + 12 hex).
fn is_virtum_vm_id(reference: &str) -> bool {
    let Some(suffix) = reference.strip_prefix("vm-") else {
        return false;
    };
    (8..=16).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

async fn require_instance(paths: &VirtumPaths, reference: &str) -> Result<Instance> {
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    metadata::resolve_instance_ref(&metadata, reference).cloned()
}

async fn wait_for_domain_inactive(vm_id: &str, timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        if !domain_exists(vm_id) {
            return true;
        }
        match domain_state(vm_id) {
            Ok(state) if !domain_is_active(&state) => return true,
            _ => {}
        }
        if started.elapsed() >= timeout {
            return false;
        }
        sleep(STOP_POLL_INTERVAL).await;
    }
}

pub async fn cleanup_failed_run(vm_id: &str, instance_dir: &Path) {
    eprintln!("Nettoyage après échec de virtum run pour '{vm_id}'…");
    port_forward::stop_instance_proxies(instance_dir);
    if let Err(err) = destroy_domain(vm_id) {
        eprintln!("  avertissement destroy: {err:#}");
    }
    if let Err(err) = undefine_domain(vm_id) {
        eprintln!("  avertissement undefine: {err:#}");
    }
    if instance_dir.exists() {
        if let Err(err) = fs::remove_dir_all(instance_dir).await {
            eprintln!(
                "  avertissement suppression {}: {err:#}",
                instance_dir.display()
            );
        }
    }
}

fn generate_vm_id() -> String {
    let id = Uuid::new_v4().simple().to_string();
    format!("vm-{}", &id[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_vm_id_has_prefix() {
        let id = generate_vm_id();
        assert!(id.starts_with("vm-"));
        assert!(id.len() > 4);
    }

    #[test]
    fn is_usable_ssh_ip_filters_loopback_and_slirp() {
        assert!(is_usable_ssh_ip("192.168.122.45"));
        assert!(!is_usable_ssh_ip("127.0.0.1"));
        assert!(!is_usable_ssh_ip("10.0.2.15"));
    }

    #[test]
    fn is_virtum_vm_id_recognizes_generated_ids() {
        assert!(is_virtum_vm_id("vm-9161ed10ea7a"));
        assert!(is_virtum_vm_id("vm-21b60413a3fa"));
        assert!(!is_virtum_vm_id("disk-test"));
        assert!(!is_virtum_vm_id("vm-short"));
    }
}
