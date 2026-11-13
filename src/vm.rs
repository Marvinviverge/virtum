//! VM lifecycle orchestration (steps 15–20).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::fs;
use tokio::time::sleep;
use uuid::Uuid;

use crate::bridge_network;
use crate::cli::{RunArgs, RunNetworkMode};
use crate::cloud_init::{build_cloud_init_iso, write_meta_data, write_user_data};
use crate::config::{self, VirtumPaths};
use crate::disk::{
    create_cow_disk, format_virtual_size_label, image_virtual_size_bytes, parse_disk_size,
    plan_disk_size,
};
use crate::images::{ensure_image, resolve_image};
use crate::lab_network;
use crate::libvirt::{
    self, define_domain, define_or_replace_domain, destroy_domain, domain_exists, domain_is_active,
    domain_state, shutdown_domain, start_domain, undefine_domain, write_domain_xml, DomainSpec,
    VirtiofsSpec, VmNic,
};
use crate::libvirt_access;
use crate::metadata::{self, Instance, InstanceStatus};
use crate::mounts::{self, ResolvedMount};
use crate::port_forward::{self, PortForward};
use crate::ssh::{
    ensure_ssh_keypair, read_public_key, run_provision_commands, ssh_auth_probe, ssh_ports_to_try,
    SshSessionOpts,
};
use crate::virtumfile::RestartPolicy;

const IP_POLL_INTERVAL: Duration = Duration::from_secs(2);
const IP_TIMEOUT: Duration = Duration::from_secs(60);
/// Premier boot + DHCP macvtap + guest-agent : plus long que le réseau libvirt NAT.
const IP_TIMEOUT_BRIDGE: Duration = Duration::from_secs(180);
const IP_BOOT_GRACE: Duration = Duration::from_secs(5);
const IP_BOOT_GRACE_BRIDGE: Duration = Duration::from_secs(15);
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
    pub port_bind: port_forward::PortBind,
    /// Réseau libvirt (`virtum-lab-…` pour un lab, sinon `default` via `None`).
    pub libvirt_network: Option<String>,
    /// Interface hôte macvtap (`virtum run --network bridge`).
    pub bridge_host_interface: Option<String>,
    /// Volumes virtiofs résolus (lab `virtum up` uniquement en v0.4).
    pub resolved_mounts: Vec<ResolvedMount>,
    /// Mode SSH (--insecure-ssh global).
    pub insecure_ssh: bool,
    /// Port sshd déclaré (Virtumfile `ssh_port`) ; 22 essayé en repli.
    pub ssh_port: u16,
    /// Démarrage au boot de l'hôte (Virtumfile `restart`).
    pub restart: Option<RestartPolicy>,
    /// MAC imposée à la carte principale (machine de lab neuve) ; `None` = libvirt la choisit.
    pub nic_mac: Option<String>,
    /// Virtumfile `ip:` (réservation déjà posée sur le réseau du lab).
    pub static_ip: Option<String>,
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
            port_bind: port_forward::PortBind::Local,
            libvirt_network: None,
            bridge_host_interface: None,
            resolved_mounts: Vec::new(),
            insecure_ssh: false,
            ssh_port: SSH_PORT,
            restart: None,
            nic_mac: None,
            static_ip: None,
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

/// Aligne l'autostart libvirt sur `restart` (VM `running` ou arrêtée volontairement).
/// Sans `restart` déclaré, l'autostart n'est pas touché (réglage manuel `virsh autostart` respecté).
pub fn sync_autostart(instance: &Instance, running: bool) -> Result<()> {
    match instance.restart {
        Some(policy) => libvirt::set_domain_autostart(&instance.id, policy.autostart(running)),
        None => Ok(()),
    }
}

/// `Instance.ssh_port` : absent pour 22 (metadata inchangée pour les labs existants).
pub fn stored_ssh_port(port: u16) -> Option<u16> {
    (port != SSH_PORT).then_some(port)
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

    let mut config = VmLaunchConfig::from_run_args(&args);
    config.insecure_ssh = args.insecure_ssh;
    if args.network == RunNetworkMode::Bridge {
        let iface =
            bridge_network::resolve_bridge_interface(args.bridge_interface.as_deref())
                .with_context(|| {
                    "Impossible d'utiliser `--network bridge`. Lancez `virtum doctor` pour diagnostiquer l'hôte."
                })?;
        crate::verr!("Réseau bridge (macvtap) sur l'interface hôte {iface}…");
        config.bridge_host_interface = Some(iface);
    }
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
            crate::verr!(
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

    let mut resolved_mounts = config.resolved_mounts.clone();
    if !resolved_mounts.is_empty() {
        mounts::ensure_virtiofs_servers(instance_dir, &mut resolved_mounts)?;
    }

    write_user_data(
        instance_dir,
        &config.user,
        public_key,
        grow_root,
        &resolved_mounts,
    )?;

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
    if !resolved_mounts.is_empty() {
        libvirt_access::refresh_virtiofs_sockets_in_dir(instance_dir)?;
    }

    if !config.port_forwards.is_empty() {
        port_forward::ensure_ports_available(&config.port_forwards, config.port_bind)?;
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        port_forward::ensure_ports_not_in_metadata(&metadata, Some(vm_id), &config.port_forwards)?;
    }

    let network_attach = resolve_network_attach(config)?;
    let mut spec = build_domain_spec(
        instance_dir,
        vm_id,
        config.ram,
        config.cpu,
        &disk_path,
        &iso_path,
        &network_attach,
        &resolved_mounts,
    );
    if spec.nic_mac.is_none() {
        spec.nic_mac = config.nic_mac.clone();
    }
    let xml_path = write_domain_xml(instance_dir, &spec)?;

    define_domain(&xml_path).with_context(|| {
        format!(
            "virsh define a échoué pour {vm_id}. \
             Vérifiez libvirt avec virtum doctor."
        )
    })?;

    if !resolved_mounts.is_empty() {
        mounts::ensure_virtiofs_servers(instance_dir, &mut resolved_mounts).with_context(|| {
            "virtiofsd n'est plus joignable avant virsh start (socket orphelin ?)"
        })?;
    }

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

    let ip_timeout = network_attach.ip_timeout();
    crate::verr!(
        "Attente de l'adresse IP ({}, max {} s)…",
        network_attach.label(),
        ip_timeout.as_secs()
    );
    let ip = match wait_for_ip(
        vm_id,
        hostname,
        &network_attach,
        ip_timeout,
        None,
        config.static_ip.as_deref(),
    )
    .await
    {
        Ok(ip) => ip,
        Err(err) => {
            if config.bridge_host_interface.is_some() {
                *retain_vm_on_error = true;
                crate::verr!(
                    "La VM '{vm_id}' reste démarrée pour diagnostic bridge. \
                     Essayez `virsh domifaddr {vm_id}` puis `virtum destroy {vm_id}`."
                );
            }
            return Err(err);
        }
    };
    let lan_ip = if network_attach.discover_lan_after_ssh() {
        try_discover_ip_bridge(vm_id).ok().flatten()
    } else {
        None
    };
    if !is_usable_ssh_ip(&ip, network_attach.prefer_non_libvirt_default()) {
        bail!(
            "Adresse IP invalide pour '{vm_id}': {ip}. \
             Vérifiez `virsh domifaddr {vm_id}` et le DHCP du réseau cible."
        );
    }

    let ssh_timeout = if grow_root {
        SSH_TIMEOUT_GROW_ROOT
    } else {
        SSH_TIMEOUT
    };
    if grow_root {
        crate::verr!(
            "Attente de SSH sur {ip} (cloud-init + agrandissement disque, max {} s)…",
            ssh_timeout.as_secs()
        );
    } else {
        crate::verr!(
            "Attente de SSH sur {ip} (cloud-init, max {} s)…",
            ssh_timeout.as_secs()
        );
    }
    let keys = ensure_ssh_keypair(&paths.keys)?;
    let private_key = keys.private_key.display();
    let ssh_opts = SshSessionOpts::new(instance_dir, config.insecure_ssh);
    let ssh_port = wait_for_ssh_login(
        &keys.private_key,
        &config.user,
        &ip,
        &ssh_ports_to_try(config.ssh_port),
        ssh_timeout,
        &ssh_opts,
    )
    .await
    .with_context(|| {
        let (strict, kh) = crate::ssh::ssh_option_strings(&ssh_opts);
        format!(
            "Instance : {}/{}\n\
             user-data : {}/user-data\n\
             Test manuel : ssh -i {private_key} -o IdentitiesOnly=yes -o {strict} -o {kh} \
             {user}@{ip}",
            paths.root.display(),
            instance_dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
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
        crate::verr!(
            "Provision post-SSH ({} commande(s), max {} s)…",
            post_boot.len(),
            SSH_TIMEOUT_PROVISION.as_secs()
        );
        run_provision_commands(
            &keys.private_key,
            &config.user,
            &ip,
            ssh_port,
            &post_boot,
            SSH_TIMEOUT_PROVISION,
            &ssh_opts,
        )?;
    } else if grow_root {
        crate::verr!("Installation qemu-guest-agent post-SSH…");
        run_provision_commands(
            &keys.private_key,
            &config.user,
            &ip,
            ssh_port,
            &["DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
               apt-get install -y -qq qemu-guest-agent && \
               systemctl enable --now qemu-guest-agent"
                .to_string()],
            SSH_TIMEOUT_PROVISION,
            &ssh_opts,
        )?;
    }

    let instance = Instance {
        id: vm_id.to_string(),
        name: config.name.clone(),
        os: config.os.clone(),
        status: InstanceStatus::Running,
        ip: Some(ip.clone()),
        ssh_port: stored_ssh_port(config.ssh_port),
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
        port_bind: if config.port_forwards.is_empty() {
            None
        } else {
            Some(config.port_bind)
        },
        bridge_host_interface: config.bridge_host_interface.clone(),
        lan_ip: lan_ip.clone(),
        mounts: if resolved_mounts.is_empty() {
            None
        } else {
            Some(mounts::mounts_to_instance_specs(&resolved_mounts))
        },
        restart: config.restart,
        static_ip: config.static_ip.clone(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    sync_autostart(&instance, true)?;
    metadata::upsert_instance(&paths.metadata, instance).await?;

    if !config.port_forwards.is_empty() {
        port_forward::start_instance_proxies(
            instance_dir,
            &ip,
            &config.port_forwards,
            config.port_bind,
        )?;
    }

    if let Err(err) = libvirt::refresh_domain_xml(
        instance_dir,
        vm_id,
        config.libvirt_network.as_deref(),
        config.bridge_host_interface.as_deref(),
    ) {
        crate::verr!("Avertissement: impossible d'enregistrer la MAC dans config.xml: {err:#}");
    }

    let shell_ref = config.name.as_deref().unwrap_or(vm_id);
    crate::vout!("VM démarrée: {vm_id}");
    if let Some(ref name) = config.name {
        crate::vout!("  Name:     {name}");
    }
    if let Some(ref lab) = config.lab_name {
        crate::vout!("  Lab:      {lab}");
    }
    crate::vout!("  OS:       {}", config.os);
    match &network_attach {
        NetworkAttach::MacvtapBridge { .. } => crate::vout!("  Réseau:   bridge (LAN)"),
        NetworkAttach::LabAndBridge { .. } => crate::vout!("  Réseau:   lab + bridge (LAN)"),
        NetworkAttach::LibvirtNetwork { name } => {
            if config.lab_name.is_some() {
                crate::vout!("  Réseau:   {name}");
            }
        }
    }
    if let Some(ref lan) = lan_ip {
        crate::vout!("  IP (lab): {ip}");
        crate::vout!("  IP (LAN): {lan}");
    } else {
        crate::vout!("  IP:       {ip}");
    }
    port_forward::print_forward_lines(&config.port_forwards, config.port_bind);
    crate::vout!("  SSH:      virtum shell {shell_ref}");
    Ok(())
}

/// Contexte de découverte d'IP selon le type de NIC.
#[derive(Debug, Clone)]
enum NetworkAttach {
    LibvirtNetwork {
        name: String,
    },
    MacvtapBridge {
        host_interface: String,
    },
    /// NIC lab (SSH) + macvtap LAN (`virtum ps` affiche les deux IP).
    LabAndBridge {
        lab_network: String,
        host_interface: String,
    },
}

impl NetworkAttach {
    fn label(&self) -> String {
        match self {
            Self::LibvirtNetwork { name } => format!("libvirt {name}"),
            Self::MacvtapBridge { host_interface } => {
                format!("bridge macvtap sur {host_interface} (LAN)")
            }
            Self::LabAndBridge {
                lab_network,
                host_interface,
            } => format!("lab {lab_network} + bridge macvtap sur {host_interface} (LAN)"),
        }
    }

    fn prefer_non_libvirt_default(&self) -> bool {
        matches!(self, Self::MacvtapBridge { .. })
    }

    fn libvirt_network_name(&self) -> Option<&str> {
        match self {
            Self::LibvirtNetwork { name } => Some(name.as_str()),
            Self::LabAndBridge { lab_network, .. } => Some(lab_network.as_str()),
            Self::MacvtapBridge { .. } => None,
        }
    }

    fn ip_timeout(&self) -> Duration {
        match self {
            Self::MacvtapBridge { .. } => IP_TIMEOUT_BRIDGE,
            Self::LabAndBridge { .. } => Duration::from_secs(120),
            Self::LibvirtNetwork { .. } => IP_TIMEOUT,
        }
    }

    fn ip_boot_grace(&self) -> Duration {
        match self {
            Self::MacvtapBridge { .. } => IP_BOOT_GRACE_BRIDGE,
            Self::LabAndBridge { .. } => IP_BOOT_GRACE,
            Self::LibvirtNetwork { .. } => IP_BOOT_GRACE,
        }
    }

    fn discover_lan_after_ssh(&self) -> bool {
        matches!(self, Self::LabAndBridge { .. })
    }
}

fn resolve_network_attach(config: &VmLaunchConfig) -> Result<NetworkAttach> {
    if let (Some(lab_net), Some(iface)) = (
        config.libvirt_network.as_ref(),
        config.bridge_host_interface.as_ref(),
    ) {
        return Ok(NetworkAttach::LabAndBridge {
            lab_network: lab_net.clone(),
            host_interface: iface.clone(),
        });
    }
    if let Some(iface) = &config.bridge_host_interface {
        return Ok(NetworkAttach::MacvtapBridge {
            host_interface: iface.clone(),
        });
    }
    let name = config
        .libvirt_network
        .clone()
        .unwrap_or_else(|| libvirt::LIBVIRT_DEFAULT_NETWORK.to_string());
    Ok(NetworkAttach::LibvirtNetwork { name })
}

#[allow(clippy::too_many_arguments)]
fn build_domain_spec<'a>(
    instance_dir: &Path,
    vm_id: &'a str,
    ram_mb: u32,
    vcpus: u32,
    disk_path: &'a Path,
    iso_path: &'a Path,
    attach: &NetworkAttach,
    resolved_mounts: &[ResolvedMount],
) -> DomainSpec<'a> {
    let nics = match attach {
        NetworkAttach::LibvirtNetwork { name } => {
            vec![VmNic::LibvirtNetwork { name: name.clone() }]
        }
        NetworkAttach::MacvtapBridge { host_interface } => vec![VmNic::MacvtapBridge {
            host_interface: host_interface.clone(),
        }],
        NetworkAttach::LabAndBridge {
            lab_network,
            host_interface,
        } => vec![
            VmNic::LibvirtNetwork {
                name: lab_network.clone(),
            },
            VmNic::MacvtapBridge {
                host_interface: host_interface.clone(),
            },
        ],
    };
    let filesystems = resolved_mounts
        .iter()
        .map(|m| VirtiofsSpec {
            tag: m.name.clone(),
            socket_path: m.socket_path.clone(),
        })
        .collect();
    DomainSpec {
        vm_id,
        ram_mb,
        vcpus,
        disk_path,
        iso_path,
        nics,
        nic_mac: libvirt::resolve_nic_mac(instance_dir, vm_id),
        filesystems,
    }
}

/// Contexte pour réutiliser une IP connue au redémarrage (metadata).
struct ResumeIpHint<'a> {
    known_ip: &'a str,
    private_key: &'a Path,
    user: &'a str,
    ssh_ports: &'a [u16],
    ssh_opts: &'a SshSessionOpts,
}

async fn wait_for_ip(
    vm_id: &str,
    hostname: &str,
    attach: &NetworkAttach,
    timeout: Duration,
    resume_hint: Option<ResumeIpHint<'_>>,
    expected_ip: Option<&str>,
) -> Result<String> {
    let started = Instant::now();
    // `ip:` déclarée : juste après un redémarrage, libvirt montre encore l'ancien bail de
    // la MAC tant que l'invité n'a pas redemandé d'adresse. On attend l'adresse déclarée ;
    // une autre adresse n'est retenue qu'au bout du délai (ancien bail d'une autre MAC).
    let mut other_ip: Option<String> = None;
    let grace = if resume_hint.is_some() {
        Duration::from_secs(2)
    } else {
        attach.ip_boot_grace()
    };
    sleep(grace).await;

    loop {
        if let Some(ip) = try_discover_ip(vm_id, hostname, attach)? {
            match expected_ip {
                Some(expected) if ip != expected => other_ip = Some(ip),
                _ => return Ok(ip),
            }
        }

        if let Some(hint) = resume_hint.as_ref() {
            // Adresse connue différente de l'adresse déclarée : pas de réutilisation.
            if expected_ip.is_none_or(|e| e == hint.known_ip)
                && is_usable_ssh_ip(hint.known_ip, attach.prefer_non_libvirt_default())
                && hint.ssh_ports.iter().any(|&port| {
                    ssh_auth_probe(
                        hint.private_key,
                        hint.user,
                        hint.known_ip,
                        port,
                        hint.ssh_opts,
                    )
                })
            {
                crate::verr!("IP réutilisée (metadata / SSH) : {}", hint.known_ip);
                return Ok(hint.known_ip.to_string());
            }
        }

        if started.elapsed() >= timeout {
            break;
        }
        sleep(IP_POLL_INTERVAL).await;
    }

    if let (Some(ip), Some(expected)) = (other_ip, expected_ip) {
        crate::verr!(
            "Avertissement: {vm_id} a obtenu {ip} au lieu de l'adresse déclarée {expected}."
        );
        return Ok(ip);
    }

    let dhcp_hint = match attach {
        NetworkAttach::LibvirtNetwork { name } | NetworkAttach::LabAndBridge { lab_network: name, .. } => {
            format!("- Vérifiez `virsh domifaddr {vm_id}` et `virsh net-dhcp-leases {name}`\n")
        }
        NetworkAttach::MacvtapBridge { host_interface } => format!(
            "- Interface hôte : {host_interface} (doit être celle de la route par défaut : `ip route`)\n\
             - Partage téléphone / Wi‑Fi : macvtap échoue souvent ; essayez `--bridge-interface <iface>`\n\
             - Vérifiez `virsh domifaddr {vm_id}` et `ip neigh` après ~2 min (cloud-init)\n"
        ),
    };

    bail!(
        "Aucune adresse IP trouvée pour '{vm_id}' après {} secondes.\n\
         Suggestions:\n\
         - Exécutez `virtum doctor`\n\
         {dhcp_hint}\
         - Relancez avec `--bridge-interface` si le partage n'utilise pas l'interface détectée",
        timeout.as_secs()
    );
}

fn try_discover_ip(vm_id: &str, hostname: &str, attach: &NetworkAttach) -> Result<Option<String>> {
    if matches!(attach, NetworkAttach::MacvtapBridge { .. }) {
        return try_discover_ip_bridge(vm_id);
    }

    let prefer_lan = false;

    for output in libvirt::domifaddr_all_sources(vm_id) {
        if let Some(ip) = libvirt::parse_ipv4_from_domifaddr(&output) {
            if is_usable_ssh_ip(&ip, prefer_lan) {
                return Ok(Some(ip));
            }
        }
    }

    if let Some(ip) = libvirt::guest_network_ipv4_from_agent(vm_id) {
        if is_usable_ssh_ip(&ip, prefer_lan) {
            return Ok(Some(ip));
        }
    }

    if let Some(net) = attach.libvirt_network_name() {
        let mac = libvirt::domiflist(vm_id)
            .ok()
            .and_then(|output| libvirt::parse_mac_from_domiflist(&output));

        if let (Some(mac), Ok(leases)) = (mac.as_ref(), libvirt::net_dhcp_leases(net)) {
            if let Some(ip) = libvirt::parse_ipv4_from_dhcp_leases(&leases, mac) {
                if is_usable_ssh_ip(&ip, prefer_lan) && dhcp_lease_mac_matches(&leases, &ip, mac) {
                    return Ok(Some(ip));
                }
            }
        }
    }

    // Pas de recherche par hostname : réutiliser `--name disk-test` reprendrait
    // une lease DHCP périmée et ferait attendre SSH sur la mauvaise IP.
    let _ = hostname;

    Ok(None)
}

fn try_discover_ip_bridge(vm_id: &str) -> Result<Option<String>> {
    for output in libvirt::domifaddr_all_sources(vm_id) {
        let ips = libvirt::parse_all_ipv4_from_domifaddr(&output);
        if let Some(ip) = libvirt::select_best_bridge_ipv4(ips) {
            if is_usable_ssh_ip(&ip, true) {
                return Ok(Some(ip));
            }
        }
    }

    if let Some(ip) = libvirt::guest_network_bridge_ipv4_from_agent(vm_id) {
        if is_usable_ssh_ip(&ip, true) {
            return Ok(Some(ip));
        }
    }

    if let Ok(output) = libvirt::domiflist(vm_id) {
        if let Some(mac) = libvirt::parse_mac_from_domiflist(&output) {
            if let Some(ip) = libvirt::neighbor_ipv4_for_mac(&mac) {
                if is_usable_ssh_ip(&ip, true) {
                    return Ok(Some(ip));
                }
            }
        }
    }

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

/// IP du réseau libvirt `default` (NAT 192.168.122.0/24).
const LIBVIRT_DEFAULT_SUBNET: &str = "192.168.122.";

fn is_usable_ssh_ip(ip: &str, bridge_lan: bool) -> bool {
    if ip.starts_with("127.")
        || ip == libvirt::SLIRP_GUEST_IPV4
        || ip == "0.0.0.0"
        || ip.starts_with("169.254.")
    {
        return false;
    }
    if bridge_lan && ip.starts_with(LIBVIRT_DEFAULT_SUBNET) {
        return false;
    }
    true
}

/// `2222` ou `2222, repli 22`.
fn format_ssh_ports(ports: &[u16]) -> String {
    match ports {
        [first, rest @ ..] if !rest.is_empty() => {
            let rest: Vec<String> = rest.iter().map(u16::to_string).collect();
            format!("{first}, repli {}", rest.join(", "))
        }
        _ => ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Attend une connexion SSH par clé sur l'un des `ports` (dans l'ordre) ; renvoie le port qui a répondu.
pub async fn wait_for_ssh_login(
    private_key: &Path,
    user: &str,
    host: &str,
    ports: &[u16],
    timeout: Duration,
    opts: &SshSessionOpts,
) -> Result<u16> {
    let started = Instant::now();
    let mut last_progress = Instant::now();
    sleep(SSH_BOOT_GRACE).await;

    loop {
        if let Some(&port) = ports
            .iter()
            .find(|&&port| ssh_auth_probe(private_key, user, host, port, opts))
        {
            return Ok(port);
        }

        if last_progress.elapsed() >= Duration::from_secs(30) {
            crate::verr!(
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
        "Connexion SSH refusée pour {user}@{host} (port {}) après {} secondes \
         (sshd pas encore joignable ; cloud-init ou growpart peut être en cours, \
         ou `ssh_port` du Virtumfile ne correspond pas au port de sshd).",
        format_ssh_ports(ports),
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
    let (libvirt_network, bridge_iface) =
        resolve_instance_network_for_refresh(&instance_dir, &instance)?;
    let mut mount_state = mounts::load_mounts_state(&instance_dir)?;
    if mount_state.is_empty() {
        if instance.mounts.as_ref().is_some_and(|m| !m.is_empty()) {
            crate::verr!(
                "Avertissement: metadata contient des mounts mais mounts-state.json est absent. \
                 Les partages virtiofs peuvent être incomplets."
            );
        }
    } else if let Err(err) = mounts::ensure_virtiofs_servers(&instance_dir, &mut mount_state) {
        crate::verr!("Avertissement: virtiofsd : {err:#}");
    }
    let xml_path = libvirt::refresh_domain_xml(
        &instance_dir,
        &vm_id,
        libvirt_network.as_deref(),
        bridge_iface.as_deref(),
    )?;
    if let Some(ref mac) = libvirt::parse_mac_address_from_xml(&std::fs::read_to_string(&xml_path)?)
    {
        crate::verr!("  Interface MAC conservée : {mac}");
    }
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

    define_or_replace_domain(&vm_id, &xml_path).with_context(|| {
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

    let network_attach =
        network_attach_from_resume(libvirt_network.as_deref(), bridge_iface.as_deref())?;
    let keys = ensure_ssh_keypair(&paths.keys)?;
    let ssh_ports = ssh_ports_to_try(instance.ssh_port.unwrap_or(SSH_PORT));
    let ssh_opts = SshSessionOpts::new(&instance_dir, false);
    let resume_hint = instance.ip.as_deref().and_then(|known_ip| {
        if is_usable_ssh_ip(known_ip, network_attach.prefer_non_libvirt_default()) {
            Some(ResumeIpHint {
                known_ip,
                private_key: &keys.private_key,
                user: &instance.user,
                ssh_ports: &ssh_ports,
                ssh_opts: &ssh_opts,
            })
        } else {
            None
        }
    });
    if resume_hint.is_some() {
        crate::verr!(
            "Attente de l'adresse IP ({}, max {} s, réutilisation metadata si joignable)…",
            network_attach.label(),
            IP_TIMEOUT.as_secs()
        );
    } else {
        crate::verr!(
            "Attente de l'adresse IP ({}, max {} s)…",
            network_attach.label(),
            IP_TIMEOUT.as_secs()
        );
    }
    let ip_timeout = network_attach.ip_timeout();
    let ip = wait_for_ip(
        &vm_id,
        &instance.hostname,
        &network_attach,
        ip_timeout,
        resume_hint,
        instance.static_ip.as_deref(),
    )
    .await?;
    if !is_usable_ssh_ip(&ip, network_attach.prefer_non_libvirt_default()) {
        bail!(
            "Adresse IP invalide pour '{vm_id}': {ip}. \
             Vérifiez `virsh domifaddr {vm_id}`."
        );
    }

    crate::verr!(
        "Attente de SSH sur {ip} (port {}, max {} s)…",
        format_ssh_ports(&ssh_ports),
        SSH_TIMEOUT.as_secs()
    );
    wait_for_ssh_login(
        &keys.private_key,
        &instance.user,
        &ip,
        &ssh_ports,
        SSH_TIMEOUT,
        &ssh_opts,
    )
    .await?;

    let lan_ip = if network_attach.discover_lan_after_ssh() {
        try_discover_ip_bridge(&vm_id).ok().flatten()
    } else {
        None
    };

    let shell_ref = instance.name.clone().unwrap_or_else(|| vm_id.clone());
    let forwards_snapshot = instance.port_forwards.clone();
    let port_bind = instance.port_bind.unwrap_or(port_forward::PortBind::Local);
    instance.status = InstanceStatus::Running;
    instance.ip = Some(ip.clone());
    instance.lan_ip = lan_ip.clone();
    sync_autostart(&instance, true)?;
    metadata::upsert_instance(&paths.metadata, instance).await?;

    if let Some(ref forwards) = forwards_snapshot {
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        port_forward::ensure_ports_not_in_metadata(&metadata, Some(&vm_id), forwards)?;
        port_forward::start_instance_proxies(&instance_dir, &ip, forwards, port_bind)?;
    }

    crate::vout!("VM démarrée: {vm_id}");
    if let Some(ref lan) = lan_ip {
        crate::vout!("  IP (lab): {ip}");
        crate::vout!("  IP (LAN): {lan}");
    } else {
        crate::vout!("  IP:       {ip}");
    }
    if let Some(ref forwards) = forwards_snapshot {
        port_forward::print_forward_lines(forwards, port_bind);
    }
    crate::vout!("  SSH:      virtum shell {shell_ref}");
    Ok(())
}

fn resolve_instance_network_for_refresh(
    instance_dir: &Path,
    instance: &Instance,
) -> Result<(Option<String>, Option<String>)> {
    let xml_path = instance_dir.join("config.xml");
    if xml_path.is_file() {
        let xml = std::fs::read_to_string(&xml_path)
            .with_context(|| format!("Impossible de lire {}", xml_path.display()))?;
        let nics = libvirt::parse_nics_from_xml(&xml);
        let lab_net = nics.iter().find_map(|nic| match nic {
            VmNic::LibvirtNetwork { name } => Some(name.clone()),
            VmNic::MacvtapBridge { .. } => None,
        });
        let bridge = nics.iter().find_map(|nic| match nic {
            VmNic::MacvtapBridge { host_interface } => Some(host_interface.clone()),
            VmNic::LibvirtNetwork { .. } => None,
        });
        if lab_net.is_some() || bridge.is_some() {
            return Ok((lab_net, bridge));
        }
    }
    let bridge = instance.bridge_host_interface.clone();
    let lab_net = if bridge.is_none() || instance.lan_ip.is_some() {
        lab_network::network_for_lab(instance.lab_name.as_deref()).ok()
    } else {
        None
    };
    Ok((lab_net, bridge))
}

fn network_attach_from_resume(
    libvirt_network: Option<&str>,
    bridge_iface: Option<&str>,
) -> Result<NetworkAttach> {
    if let (Some(lab), Some(iface)) = (libvirt_network, bridge_iface) {
        return Ok(NetworkAttach::LabAndBridge {
            lab_network: lab.to_string(),
            host_interface: iface.to_string(),
        });
    }
    if let Some(iface) = bridge_iface {
        return Ok(NetworkAttach::MacvtapBridge {
            host_interface: iface.to_string(),
        });
    }
    let name = libvirt_network
        .map(|s| s.to_string())
        .unwrap_or_else(|| libvirt::LIBVIRT_DEFAULT_NETWORK.to_string());
    Ok(NetworkAttach::LibvirtNetwork { name })
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
            crate::verr!(
                "La VM '{vm_id}' ne s'est pas arrêtée en {} s ; arrêt forcé (virsh destroy).",
                STOP_TIMEOUT.as_secs()
            );
            destroy_domain(&vm_id)
                .with_context(|| format!("virsh destroy a échoué pour {vm_id}"))?;
        }
    } else {
        crate::verr!(
            "Avertissement: le domaine libvirt '{vm_id}' est introuvable ; \
             mise à jour du statut metadata uniquement."
        );
    }

    let instance_dir = paths.instances.join(&vm_id);
    port_forward::stop_instance_proxies(&instance_dir);

    instance.status = InstanceStatus::Stopped;
    if domain_exists(&vm_id) {
        sync_autostart(&instance, false)?;
    }
    metadata::upsert_instance(&paths.metadata, instance).await?;

    crate::vout!("VM arrêtée: {vm_id}");
    Ok(())
}

/// Arrêt immédiat (`virsh destroy`) : pour `snapshot restore`, où l'état courant est abandonné.
pub async fn kill_vm_async(reference: &str) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let mut instance = require_instance(&paths, reference).await?;
    let vm_id = instance.id.clone();

    destroy_domain(&vm_id).with_context(|| format!("virsh destroy a échoué pour {vm_id}"))?;
    port_forward::stop_instance_proxies(&paths.instances.join(&vm_id));

    instance.status = InstanceStatus::Stopped;
    if domain_exists(&vm_id) {
        sync_autostart(&instance, false)?;
    }
    metadata::upsert_instance(&paths.metadata, instance).await?;

    crate::vout!("VM arrêtée (forcé): {vm_id}");
    Ok(())
}

pub fn destroy_vm(reference: &str, purge_mounts: bool) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(destroy_vm_async(reference, purge_mounts))
}

pub async fn destroy_vm_async(reference: &str, purge_mounts: bool) -> Result<()> {
    let paths = config::ensure_layout().await?;
    let (vm_id, orphan) = resolve_destroy_target(&paths, reference).await?;
    let instance_dir = paths.instances.join(&vm_id);

    if orphan {
        crate::verr!("Domaine Virtum orphelin (libvirt sans entrée metadata.json) : {vm_id}");
    }

    port_forward::stop_instance_proxies(&instance_dir);

    // Réservation `ip:` du lab : sinon l'adresse resterait bloquée pour cette MAC.
    // Lue avant l'undefine (la MAC vient de config.xml ou du domaine).
    let lab_reservation = {
        let metadata = metadata::load_metadata(&paths.metadata).await?;
        metadata::find_instance(&metadata, &vm_id)
            .and_then(|inst| inst.lab_name.clone())
            .and_then(|lab| lab_network::lab_network_name(&lab).ok())
            .zip(libvirt::resolve_nic_mac(&instance_dir, &vm_id))
    };

    if domain_exists(&vm_id) {
        if domain_is_active(&domain_state(&vm_id)?) {
            destroy_domain(&vm_id)
                .with_context(|| format!("virsh destroy a échoué pour {vm_id}"))?;
        }
        undefine_domain(&vm_id).with_context(|| format!("virsh undefine a échoué pour {vm_id}"))?;
    } else {
        crate::verr!("Avertissement: le domaine libvirt '{vm_id}' est déjà absent.");
    }

    let purge_dirs = if purge_mounts && instance_dir.is_dir() {
        mounts::host_dirs_from_instance(&instance_dir)
    } else {
        Vec::new()
    };

    if let Some((net, mac)) = lab_reservation {
        match lab_network::remove_dhcp_reservations(&net, &mac) {
            Ok(0) => {}
            Ok(_) => crate::verr!("Réservation DHCP retirée du réseau {net}."),
            Err(err) => crate::verr!("Avertissement: réservation DHCP non retirée ({err:#})."),
        }
    }

    if instance_dir.exists() {
        mounts::teardown_virtiofs(&instance_dir);
        fs::remove_dir_all(&instance_dir)
            .await
            .with_context(|| format!("Impossible de supprimer {}", instance_dir.display()))?;
    }

    if purge_mounts && !purge_dirs.is_empty() {
        let root = mounts::infer_mount_purge_root(&purge_dirs)?;
        let n = mounts::purge_host_mount_dirs(&purge_dirs, &root)?;
        if n == 0 {
            crate::verr!("Avertissement: --purge n'a supprimé aucun dossier (déjà absents).");
        }
    }

    let metadata = metadata::load_metadata(&paths.metadata).await?;
    if metadata::find_instance(&metadata, &vm_id).is_some() {
        metadata::remove_instance(&paths.metadata, &vm_id).await?;
    }

    crate::vout!("VM supprimée: {vm_id}");
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
         `virtum destroy vm-…` si un déploiement a échoué à mi-chemin, ou relancez `virtum up` \
         (les répertoires orphelins sont nettoyés automatiquement)."
    );
}

/// Id libvirt généré par Virtum (`vm-` + 12 hex).
pub fn is_virtum_vm_id(reference: &str) -> bool {
    let Some(suffix) = reference.strip_prefix("vm-") else {
        return false;
    };
    (8..=16).contains(&suffix.len()) && suffix.chars().all(|c| c.is_ascii_hexdigit())
}

/// Aligne `status` sur l'état libvirt réel et persiste l'écart dans `metadata.json`.
pub async fn refresh_instance_status(
    metadata_path: &Path,
    mut instance: Instance,
) -> Result<Instance> {
    let domain_active = if domain_exists(&instance.id) {
        Some(domain_is_active(&domain_state(&instance.id)?))
    } else {
        None
    };
    let live = metadata::reconcile_status(instance.status, domain_active);
    if live != instance.status {
        instance.status = live;
        metadata::upsert_instance(metadata_path, instance.clone()).await?;
    }
    Ok(instance)
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
    crate::verr!("Nettoyage après échec de virtum run pour '{vm_id}'…");
    mounts::teardown_virtiofs(instance_dir);
    port_forward::stop_instance_proxies(instance_dir);
    if let Err(err) = destroy_domain(vm_id) {
        crate::verr!("  avertissement destroy: {err:#}");
    }
    if let Err(err) = undefine_domain(vm_id) {
        crate::verr!("  avertissement undefine: {err:#}");
    }
    if instance_dir.exists() {
        if let Err(err) = fs::remove_dir_all(instance_dir).await {
            crate::verr!(
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
    fn ssh_port_storage_and_label() {
        assert_eq!(stored_ssh_port(22), None);
        assert_eq!(stored_ssh_port(2222), Some(2222));
        assert_eq!(format_ssh_ports(&[22]), "22");
        assert_eq!(format_ssh_ports(&[2222, 22]), "2222, repli 22");
    }

    #[test]
    fn generate_vm_id_has_prefix() {
        let id = generate_vm_id();
        assert!(id.starts_with("vm-"));
        assert!(id.len() > 4);
    }

    #[test]
    fn is_usable_ssh_ip_filters_loopback_and_slirp() {
        assert!(is_usable_ssh_ip("192.168.122.45", false));
        assert!(is_usable_ssh_ip("192.168.1.50", true));
        assert!(!is_usable_ssh_ip("192.168.122.45", true));
        assert!(!is_usable_ssh_ip("127.0.0.1", false));
        assert!(!is_usable_ssh_ip("10.0.2.15", false));
    }

    #[test]
    fn is_virtum_vm_id_recognizes_generated_ids() {
        assert!(is_virtum_vm_id("vm-9161ed10ea7a"));
        assert!(is_virtum_vm_id("vm-21b60413a3fa"));
        assert!(!is_virtum_vm_id("disk-test"));
        assert!(!is_virtum_vm_id("vm-short"));
    }
}
