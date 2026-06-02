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
use crate::disk::create_cow_disk;
use crate::images::{ensure_image, resolve_image};
use crate::libvirt::{
    self, define_domain, destroy_domain, domain_exists, domain_is_active, domain_state,
    shutdown_domain, start_domain, undefine_domain, write_domain_xml, DomainSpec,
};
use crate::libvirt_access;
use crate::metadata::{self, Instance, InstanceStatus};
use crate::ssh::{ensure_ssh_keypair, read_public_key, ssh_auth_probe};

const IP_POLL_INTERVAL: Duration = Duration::from_secs(2);
const IP_TIMEOUT: Duration = Duration::from_secs(60);
const IP_BOOT_GRACE: Duration = Duration::from_secs(5);
const SSH_POLL_INTERVAL: Duration = Duration::from_secs(2);
const SSH_TIMEOUT: Duration = Duration::from_secs(120);
const SSH_BOOT_GRACE: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_PORT: u16 = 22;

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
    let keys = ensure_ssh_keypair(&paths.keys)?;
    let public_key = read_public_key(&keys.public_key)?;

    let vm_id = generate_vm_id();
    if domain_exists(&vm_id) {
        bail!(
            "Collision de nom libvirt improbable mais détectée pour '{vm_id}'. Relancez virtum run."
        );
    }

    let hostname = args.hostname.clone().unwrap_or_else(|| vm_id.clone());
    let instance_dir = paths.instances.join(&vm_id);

    let mut retain_vm_on_error = false;
    if let Err(err) = run_vm_inner(
        &args,
        &paths,
        &instance_dir,
        &vm_id,
        &hostname,
        &public_key,
        &mut retain_vm_on_error,
    )
    .await
    {
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

async fn run_vm_inner(
    args: &RunArgs,
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

    let image = resolve_image(&args.os, &paths.images)?;
    let base_image = ensure_image(&image).await?;

    let disk_path = instance_dir.join("disk.qcow2");
    create_cow_disk(&base_image, &disk_path)?;

    write_user_data(instance_dir, &args.user, public_key)?;
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

    let spec = DomainSpec {
        vm_id,
        ram_mb: args.ram,
        vcpus: args.cpu,
        disk_path: &disk_path,
        iso_path: &iso_path,
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
        libvirt::LIBVIRT_DEFAULT_NETWORK,
        IP_TIMEOUT.as_secs()
    );
    let ip = wait_for_ip(vm_id, hostname, IP_TIMEOUT).await?;
    if !is_usable_ssh_ip(&ip) {
        bail!(
            "Adresse IP invalide pour '{vm_id}': {ip}. \
             Attendu une IP du réseau libvirt (ex. 192.168.122.x)."
        );
    }

    eprintln!(
        "Attente de SSH sur {ip} (cloud-init, max {} s)…",
        SSH_TIMEOUT.as_secs()
    );
    let keys = ensure_ssh_keypair(&paths.keys)?;
    wait_for_ssh_login(&keys.private_key, &args.user, &ip, SSH_PORT, SSH_TIMEOUT).await?;
    *retain_vm_on_error = true;

    let instance = Instance {
        id: vm_id.to_string(),
        os: args.os.clone(),
        status: InstanceStatus::Running,
        ip: Some(ip.clone()),
        ssh_port: None,
        user: args.user.clone(),
        hostname: hostname.to_string(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    metadata::upsert_instance(&paths.metadata, instance).await?;

    println!("VM démarrée: {vm_id}");
    println!("  OS:       {}", args.os);
    println!("  IP:       {ip}");
    println!("  SSH:      virtum shell {vm_id}");
    Ok(())
}

pub async fn wait_for_ip(vm_id: &str, hostname: &str, timeout: Duration) -> Result<String> {
    let started = Instant::now();
    sleep(IP_BOOT_GRACE).await;

    loop {
        if let Some(ip) = try_discover_ip(vm_id, hostname)? {
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
         - Exécutez `virtum doctor` (réseau « default » actif)\n\
         - Vérifiez `virsh domifaddr {vm_id}` et `virsh net-dhcp-leases default`\n\
         - Attendez la fin de cloud-init puis réessayez",
        timeout.as_secs()
    );
}

fn try_discover_ip(vm_id: &str, hostname: &str) -> Result<Option<String>> {
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

    if let Ok(leases) = libvirt::net_dhcp_leases(libvirt::LIBVIRT_DEFAULT_NETWORK) {
        if let Some(ref mac) = mac {
            if let Some(ip) = libvirt::parse_ipv4_from_dhcp_leases(&leases, mac) {
                if is_usable_ssh_ip(&ip) {
                    return Ok(Some(ip));
                }
            }
        }
        if let Some(ip) = libvirt::parse_ipv4_from_dhcp_leases_by_hostname(&leases, hostname) {
            if is_usable_ssh_ip(&ip) {
                return Ok(Some(ip));
            }
        }
    }

    Ok(None)
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
    sleep(SSH_BOOT_GRACE).await;

    loop {
        if ssh_auth_probe(private_key, user, host, port) {
            return Ok(());
        }

        if started.elapsed() >= timeout {
            break;
        }
        sleep(SSH_POLL_INTERVAL).await;
    }

    bail!(
        "Connexion SSH refusée pour {user}@{host}:{port} après {} secondes.\n\
         Suggestions:\n\
         - Premier boot : attendez la fin de cloud-init (1–2 min)\n\
         - Vérifiez `~/.virtum/instances/<vm-id>/user-data`\n\
         - Test : ssh -i ~/.virtum/keys/virtum_id_ed25519 -o IdentitiesOnly=yes {user}@{host}",
        timeout.as_secs()
    );
}

pub fn start_vm(vm_id: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(start_vm_async(vm_id))
}

pub async fn start_vm_async(vm_id: &str) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let mut instance = require_instance(&paths, vm_id).await?;

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
            if domain_exists(vm_id) {
                let state = domain_state(vm_id)?;
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

    let instance_dir = paths.instances.join(vm_id);
    if !instance_dir.is_dir() {
        bail!(
            "Répertoire instance introuvable: {}. \
             La VM a peut-être été supprimée manuellement ; utilisez `virtum destroy {vm_id}` puis `virtum run`.",
            instance_dir.display()
        );
    }

    let disk_path = instance_dir.join("disk.qcow2");
    let iso_path = instance_dir.join("cloud-init.iso");
    let xml_path = instance_dir.join("config.xml");
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

    if !domain_exists(vm_id) {
        define_domain(&xml_path).with_context(|| {
            format!(
                "virsh define a échoué pour {vm_id} (domaine absent). \
                 Vérifiez {} et virtum doctor.",
                xml_path.display()
            )
        })?;
    }

    start_domain(vm_id).with_context(|| {
        format!(
            "virsh start a échoué pour {vm_id}.\n{}",
            libvirt_access::permission_denied_hint(&paths)
        )
    })?;

    let state = domain_state(vm_id)?;
    if state != "running" {
        bail!("La VM {vm_id} n'est pas en état running (état actuel: {state}).");
    }

    eprintln!(
        "Attente de l'adresse IP (réseau {}, max {} s)…",
        libvirt::LIBVIRT_DEFAULT_NETWORK,
        IP_TIMEOUT.as_secs()
    );
    let ip = wait_for_ip(vm_id, &instance.hostname, IP_TIMEOUT).await?;
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

    instance.status = InstanceStatus::Running;
    instance.ip = Some(ip.clone());
    metadata::upsert_instance(&paths.metadata, instance).await?;

    println!("VM démarrée: {vm_id}");
    println!("  IP:       {ip}");
    println!("  SSH:      virtum shell {vm_id}");
    Ok(())
}

pub fn stop_vm(vm_id: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(stop_vm_async(vm_id))
}

pub async fn stop_vm_async(vm_id: &str) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let mut instance = require_instance(&paths, vm_id).await?;

    if domain_exists(vm_id) {
        shutdown_domain(vm_id).with_context(|| format!("virsh shutdown a échoué pour {vm_id}"))?;
        if !wait_for_domain_inactive(vm_id, STOP_TIMEOUT).await {
            eprintln!(
                "La VM '{vm_id}' ne s'est pas arrêtée en {} s ; arrêt forcé (virsh destroy).",
                STOP_TIMEOUT.as_secs()
            );
            destroy_domain(vm_id)
                .with_context(|| format!("virsh destroy a échoué pour {vm_id}"))?;
        }
    } else {
        eprintln!(
            "Avertissement: le domaine libvirt '{vm_id}' est introuvable ; \
             mise à jour du statut metadata uniquement."
        );
    }

    instance.status = InstanceStatus::Stopped;
    metadata::upsert_instance(&paths.metadata, instance).await?;

    println!("VM arrêtée: {vm_id}");
    Ok(())
}

pub fn destroy_vm(vm_id: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(destroy_vm_async(vm_id))
}

pub async fn destroy_vm_async(vm_id: &str) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let _instance = require_instance(&paths, vm_id).await?;
    let instance_dir = paths.instances.join(vm_id);

    destroy_domain(vm_id).with_context(|| format!("virsh destroy a échoué pour {vm_id}"))?;
    undefine_domain(vm_id).with_context(|| format!("virsh undefine a échoué pour {vm_id}"))?;

    if instance_dir.exists() {
        fs::remove_dir_all(&instance_dir)
            .await
            .with_context(|| format!("Impossible de supprimer {}", instance_dir.display()))?;
    }

    metadata::remove_instance(&paths.metadata, vm_id).await?;

    println!("VM supprimée: {vm_id}");
    Ok(())
}

async fn require_instance(paths: &VirtumPaths, vm_id: &str) -> Result<Instance> {
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    metadata::find_instance(&metadata, vm_id)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Instance '{vm_id}' introuvable dans metadata.json. \
                 Lancez `virtum run` ou listez les VM avec `virtum ps -a`."
            )
        })
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
}
