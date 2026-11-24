//! libvirt domain XML and virsh wrappers (steps 13–14).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::command::{path_arg, run_command};
use crate::tr;

/// Réseau NAT libvirt v0.1 (typiquement 192.168.122.0/24).
pub const LIBVIRT_DEFAULT_NETWORK: &str = "default";

/// Ancienne IP SLIRP (instances créées avant le réseau `default`).
pub const SLIRP_GUEST_IPV4: &str = "10.0.2.15";

/// Libvirt utilisé par Virtum : toujours le libvirt système. Sans `-c`, virsh suit
/// `LIBVIRT_DEFAULT_URI`, que seul Ubuntu pose (et seulement dans un shell de connexion) ;
/// ailleurs (Debian, cron, `ssh hôte virtum …`) il se connecterait à `qemu:///session`.
pub const LIBVIRT_URI: &str = "qemu:///system";

/// `virsh -c qemu:///system` : seule façon autorisée de lancer virsh dans Virtum.
pub fn virsh_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("virsh");
    cmd.args(["-c", LIBVIRT_URI]);
    cmd
}

/// Arguments complets d'un appel virsh (connexion explicite au libvirt système).
pub fn virsh_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec!["-c", LIBVIRT_URI];
    all.extend_from_slice(args);
    all
}

fn run_virsh(args: &[&str]) -> Result<std::process::Output> {
    run_command("virsh", &virsh_args(args))
}

const QEMU_MACHINE: &str = "pc-q35-7.2";

/// Connexion réseau d'une NIC invitée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmNic {
    /// Réseau libvirt NAT (`default`, `virtum-lab-…`).
    LibvirtNetwork { name: String },
    /// Macvtap en mode bridge sur l'interface physique (LAN bureau).
    MacvtapBridge { host_interface: String },
}

/// Partage virtiofs (socket virtiofsd + tag invité).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtiofsSpec {
    pub tag: String,
    pub socket_path: PathBuf,
}

pub struct DomainSpec<'a> {
    pub vm_id: &'a str,
    pub ram_mb: u32,
    pub vcpus: u32,
    pub disk_path: &'a Path,
    pub iso_path: &'a Path,
    pub nics: Vec<VmNic>,
    /// MAC de la première NIC (évite changement d'IP DHCP après `undefine`/`define`).
    pub nic_mac: Option<String>,
    pub filesystems: Vec<VirtiofsSpec>,
}

impl<'a> DomainSpec<'a> {
    /// Une seule NIC sur un réseau libvirt (v0.1 / lab NAT).
    pub fn with_libvirt_network(
        vm_id: &'a str,
        ram_mb: u32,
        vcpus: u32,
        disk_path: &'a Path,
        iso_path: &'a Path,
        network: &str,
    ) -> Self {
        Self {
            vm_id,
            ram_mb,
            vcpus,
            disk_path,
            iso_path,
            nics: vec![VmNic::LibvirtNetwork {
                name: network.to_string(),
            }],
            nic_mac: None,
            filesystems: Vec::new(),
        }
    }
}

fn render_filesystem_xml(fs: &VirtiofsSpec) -> Result<String> {
    let socket = socket_absolute_path(&fs.socket_path)?;
    Ok(format!(
        r#"    <filesystem type='mount' accessmode='mapped'>
      <driver type='virtiofs' queue='1024'/>
      <source socket='{}'/>
      <target dir='{}'/>
    </filesystem>"#,
        xml_escape(&socket),
        xml_escape(&fs.tag)
    ))
}

fn render_interface_xml(nic: &VmNic, mac: Option<&str>) -> String {
    let mac_line = mac
        .map(|m| format!("\n      <mac address='{}'/>", xml_escape(m)))
        .unwrap_or_default();
    match nic {
        VmNic::LibvirtNetwork { name } => format!(
            r#"    <interface type='network'>{mac_line}
      <source network='{}'/>
      <model type='virtio'/>
    </interface>"#,
            xml_escape(name)
        ),
        VmNic::MacvtapBridge { host_interface } => format!(
            r#"    <interface type='direct'>{mac_line}
      <source dev='{}' mode='bridge'/>
      <model type='virtio'/>
    </interface>"#,
            xml_escape(host_interface)
        ),
    }
}

/// Builds libvirt domain XML for a headless KVM guest.
// rustfmt garderait `/* i18n-ok */` sur sa propre ligne, hors de celle du modèle XML.
#[rustfmt::skip]
pub fn generate_domain_xml(spec: &DomainSpec<'_>) -> Result<String> {
    if spec.nics.is_empty() {
        bail!(tr!(
            fr: "DomainSpec nécessite au moins une interface réseau.",
            en: "DomainSpec requires at least one network interface.",
        ));
    }
    let disk = absolute_path(spec.disk_path)?;
    let iso = absolute_path(spec.iso_path)?;
    let mac = spec.nic_mac.as_deref();
    let interfaces: String = spec
        .nics
        .iter()
        .enumerate()
        .map(|(i, nic)| {
            let nic_mac = if i == 0 { mac } else { None };
            render_interface_xml(nic, nic_mac)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let filesystems: String = spec
        .filesystems
        .iter()
        .map(render_filesystem_xml)
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    let fs_block = if filesystems.is_empty() {
        String::new()
    } else {
        format!("{filesystems}\n")
    };
    // virtiofs exige de la RAM partagée hôte↔invité (libvirt/QEMU).
    let (memory_backing, seclabel) = if spec.filesystems.is_empty() {
        (String::new(), String::new())
    } else {
        // Les profils AppArmor dynamiques de libvirt listent disque/ISO mais pas le socket
        // virtiofsd ; sans cela QEMU obtient « Permission denied » sur le chardev socket.
        (
            "  <memoryBacking>\n    <source type='memfd'/>\n    <access mode='shared'/>\n  </memoryBacking>\n"
                .to_string(),
            "  <seclabel type='none' model='none'/>\n".to_string(),
        )
    };
    Ok(format!(
        /* i18n-ok */ r#"<domain type='kvm'>
  <name>{name}</name>
{owner}  <memory unit='MiB'>{ram}</memory>
  <vcpu>{vcpus}</vcpu>
{seclabel}{memory_backing}  <os>
    <type arch='x86_64' machine='{machine}'>hvm</type>
    <boot dev='hd'/>
  </os>
  <features>
    <acpi/>
    <apic/>
  </features>
  <cpu mode='host-passthrough'/>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>destroy</on_crash>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' discard='unmap'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='{iso}'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
{interfaces}
{filesystems}
    <console type='pty'>
      <target type='serial' port='0'/>
    </console>
    <!-- Carte graphique sans affichage : sans elle, le GRUB des images Debian 13 fait
         redémarrer la VM en boucle avant de charger le noyau. -->
    <video>
      <model type='vga'/>
    </video>
    <channel type='unix'>
      <target type='virtio' name='org.qemu.guest_agent.0'/>
    </channel>
    <rng model='virtio'>
      <backend model='random'>/dev/urandom</backend>
    </rng>
  </devices>
</domain>
"#,
        name = xml_escape(spec.vm_id),
        ram = spec.ram_mb,
        vcpus = spec.vcpus,
        seclabel = seclabel,
        owner = crate::ownership::metadata_xml("  "),
        memory_backing = memory_backing,
        machine = QEMU_MACHINE,
        disk = xml_escape(&disk),
        iso = xml_escape(&iso),
        interfaces = interfaces,
        filesystems = fs_block,
    ))
}

/// Lit les entrées virtiofs depuis un XML de domaine.
pub fn parse_filesystems_from_xml(xml: &str) -> Vec<VirtiofsSpec> {
    let mut out = Vec::new();
    let mut in_fs = false;
    let mut tag = None;
    let mut socket = None;
    for line in xml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<filesystem ") {
            in_fs = true;
            tag = None;
            socket = None;
            continue;
        }
        if in_fs {
            if trimmed.starts_with("<target ") {
                tag = xml_attribute_value(trimmed, "dir");
            }
            if trimmed.starts_with("<source ") {
                socket = xml_attribute_value(trimmed, "socket");
            }
            if trimmed == "</filesystem>" {
                if let (Some(tag), Some(socket)) = (tag.take(), socket.take()) {
                    out.push(VirtiofsSpec {
                        tag,
                        socket_path: PathBuf::from(socket),
                    });
                }
                in_fs = false;
            }
        }
    }
    out
}

fn filesystems_for_instance(instance_dir: &Path) -> Result<Vec<VirtiofsSpec>> {
    let mounts = crate::mounts::load_mounts_state(instance_dir)?;
    if !mounts.is_empty() {
        return Ok(mounts
            .iter()
            .map(|m| VirtiofsSpec {
                tag: m.name.clone(),
                socket_path: m.socket_path.clone(),
            })
            .collect());
    }
    let xml_path = instance_dir.join("config.xml");
    if xml_path.is_file() {
        let content = std::fs::read_to_string(&xml_path)?;
        let parsed = parse_filesystems_from_xml(&content);
        if !parsed.is_empty() {
            return Ok(parsed);
        }
    }
    Ok(Vec::new())
}

/// Lit `<mac address='…'/>` depuis un XML de domaine.
pub fn parse_mac_address_from_xml(xml: &str) -> Option<String> {
    for line in xml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<mac ") {
            if let Some(mac) = xml_attribute_value(trimmed, "address") {
                let mac = mac.to_lowercase();
                if mac.len() == 17 && mac.matches(':').count() == 5 {
                    return Some(mac);
                }
            }
        }
    }
    None
}

/// Conserve la MAC de la VM (config.xml ou domaine libvirt existant).
pub fn resolve_nic_mac(instance_dir: &Path, vm_id: &str) -> Option<String> {
    let xml_path = instance_dir.join("config.xml");
    if xml_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&xml_path) {
            if let Some(mac) = parse_mac_address_from_xml(&content) {
                return Some(mac);
            }
        }
    }
    if domain_exists(vm_id) {
        if let Ok(output) = run_virsh(&["domiflist", vm_id]) {
            if let Some(mac) =
                parse_mac_from_domiflist(String::from_utf8_lossy(&output.stdout).as_ref())
            {
                return Some(mac);
            }
        }
    }
    None
}

/// Lit le réseau libvirt depuis `config.xml` (première NIC type `network`).
pub fn parse_network_from_xml(xml: &str) -> Option<String> {
    parse_nics_from_xml(xml)
        .into_iter()
        .find_map(|nic| match nic {
            VmNic::LibvirtNetwork { name } => Some(name),
            VmNic::MacvtapBridge { .. } => None,
        })
}

/// Lit la première interface macvtap (`type='direct'`) depuis `config.xml`.
pub fn parse_macvtap_interface_from_xml(xml: &str) -> Option<String> {
    parse_nics_from_xml(xml)
        .into_iter()
        .find_map(|nic| match nic {
            VmNic::MacvtapBridge { host_interface } => Some(host_interface),
            VmNic::LibvirtNetwork { .. } => None,
        })
}

/// Parse les blocs `<interface>` du domain XML.
pub fn parse_nics_from_xml(xml: &str) -> Vec<VmNic> {
    let mut nics = Vec::new();
    let mut in_interface = false;
    let mut iface_type: Option<String> = None;
    let mut network: Option<String> = None;
    let mut direct_dev: Option<String> = None;
    let mut direct_mode: Option<String> = None;

    let flush = |nics: &mut Vec<VmNic>,
                 in_interface: &mut bool,
                 iface_type: &mut Option<String>,
                 network: &mut Option<String>,
                 direct_dev: &mut Option<String>,
                 direct_mode: &mut Option<String>| {
        if !*in_interface {
            return;
        }
        match iface_type.as_deref() {
            Some("network") => {
                if let Some(name) = network.take() {
                    nics.push(VmNic::LibvirtNetwork { name });
                }
            }
            Some("direct") => {
                if let (Some(dev), Some(mode)) = (direct_dev.take(), direct_mode.take()) {
                    if mode == "bridge" {
                        nics.push(VmNic::MacvtapBridge {
                            host_interface: dev,
                        });
                    }
                }
            }
            _ => {}
        }
        *in_interface = false;
        *iface_type = None;
    };

    for line in xml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<interface ") {
            flush(
                &mut nics,
                &mut in_interface,
                &mut iface_type,
                &mut network,
                &mut direct_dev,
                &mut direct_mode,
            );
            in_interface = true;
            if let Some(t) = trimmed
                .strip_prefix("<interface type=")
                .and_then(parse_xml_quoted_value)
            {
                iface_type = Some(t);
            }
            continue;
        }
        if !in_interface {
            continue;
        }
        if trimmed.starts_with("</interface>") {
            flush(
                &mut nics,
                &mut in_interface,
                &mut iface_type,
                &mut network,
                &mut direct_dev,
                &mut direct_mode,
            );
            continue;
        }
        if trimmed.starts_with("<source ") {
            if let Some(name) = xml_attribute_value(trimmed, "network") {
                network = Some(name);
            }
            if let Some(dev) = xml_attribute_value(trimmed, "dev") {
                direct_dev = Some(dev);
            }
            if let Some(mode) = xml_attribute_value(trimmed, "mode") {
                direct_mode = Some(mode);
            }
        }
    }
    flush(
        &mut nics,
        &mut in_interface,
        &mut iface_type,
        &mut network,
        &mut direct_dev,
        &mut direct_mode,
    );
    nics
}

fn parse_xml_quoted_value(rest: &str) -> Option<String> {
    let quoted = rest.trim_start_matches('=').trim();
    let quoted = quoted.trim_start_matches('\'').trim_start_matches('"');
    let end = quoted.find(['\'', '"'])?;
    Some(quoted[..end].to_string())
}

fn xml_attribute_value(line: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=");
    let rest = line.split(&needle).nth(1)?;
    parse_xml_quoted_value(rest)
}

pub fn write_domain_xml(instance_dir: &Path, spec: &DomainSpec<'_>) -> Result<PathBuf> {
    let xml = generate_domain_xml(spec)?;
    let xml_path = instance_dir.join("config.xml");
    // Atomique et sans suivre de lien : un config.xml remplacé par un lien symbolique ne
    // peut pas rediriger l'écriture vers un autre fichier de l'utilisateur.
    crate::fsutil::write_atomic(&xml_path, xml.as_bytes(), 0o644)?;
    Ok(xml_path)
}

/// Réécrit `config.xml` en conservant RAM/CPU ; réseau selon les arguments ou l'existant.
pub fn refresh_domain_xml(
    instance_dir: &Path,
    vm_id: &str,
    libvirt_network: Option<&str>,
    bridge_host_interface: Option<&str>,
) -> Result<PathBuf> {
    let xml_path = instance_dir.join("config.xml");
    let disk_path = instance_dir.join("disk.qcow2");
    let iso_path = instance_dir.join("cloud-init.iso");

    let (ram_mb, vcpus) = if xml_path.is_file() {
        let content = std::fs::read_to_string(&xml_path).with_context(
            || tr!(fr: "Impossible de lire {}", en: "Cannot read {}", xml_path.display()),
        )?;
        parse_ram_vcpu_from_xml(&content).unwrap_or((2048, 2))
    } else {
        (2048, 2)
    };

    let nics = nics_from_refresh_args(libvirt_network, bridge_host_interface, &xml_path)?;
    let nic_mac = resolve_nic_mac(instance_dir, vm_id);
    let filesystems = filesystems_for_instance(instance_dir)?;
    let spec = DomainSpec {
        vm_id,
        ram_mb,
        vcpus,
        disk_path: &disk_path,
        iso_path: &iso_path,
        nics,
        nic_mac,
        filesystems,
    };
    write_domain_xml(instance_dir, &spec)
}

fn nics_from_refresh_args(
    libvirt_network: Option<&str>,
    bridge_host_interface: Option<&str>,
    xml_path: &Path,
) -> Result<Vec<VmNic>> {
    if let (Some(net), Some(iface)) = (libvirt_network, bridge_host_interface) {
        return Ok(vec![
            VmNic::LibvirtNetwork {
                name: net.to_string(),
            },
            VmNic::MacvtapBridge {
                host_interface: iface.to_string(),
            },
        ]);
    }
    if let Some(iface) = bridge_host_interface {
        return Ok(vec![VmNic::MacvtapBridge {
            host_interface: iface.to_string(),
        }]);
    }
    if let Some(net) = libvirt_network {
        return Ok(vec![VmNic::LibvirtNetwork {
            name: net.to_string(),
        }]);
    }
    if xml_path.is_file() {
        let content = std::fs::read_to_string(xml_path).with_context(
            || tr!(fr: "Impossible de lire {}", en: "Cannot read {}", xml_path.display()),
        )?;
        let parsed = parse_nics_from_xml(&content);
        if !parsed.is_empty() {
            return Ok(parsed);
        }
    }
    Ok(vec![VmNic::LibvirtNetwork {
        name: LIBVIRT_DEFAULT_NETWORK.to_string(),
    }])
}

/// RAM (MiB) et vCPU lus depuis `config.xml` d'une instance.
pub fn read_ram_vcpu_from_instance(instance_dir: &Path) -> Result<(u32, u32)> {
    let xml_path = instance_dir.join("config.xml");
    let content = std::fs::read_to_string(&xml_path).with_context(
        || tr!(fr: "Impossible de lire {}", en: "Cannot read {}", xml_path.display()),
    )?;
    parse_ram_vcpu_from_xml(&content).ok_or_else(|| {
        anyhow::anyhow!(tr!(
            fr: "Impossible de lire RAM/vCPU dans {}",
            en: "Cannot read RAM/vCPU from {}",
            xml_path.display(),
        ))
    })
}

/// RAM (MiB) et vCPU de la définition **persistante** du domaine (`virsh dumpxml --inactive`).
///
/// Reflète ce qui s'appliquera au prochain démarrage, y compris un `virsh setmem --config`
/// ou `virsh edit` fait hors de Virtum.
pub fn read_ram_vcpu_from_domain(vm_id: &str) -> Result<(u32, u32)> {
    let output = run_virsh(&["dumpxml", "--inactive", vm_id])?;
    let xml = String::from_utf8_lossy(&output.stdout);
    parse_ram_vcpu_from_xml(&xml).ok_or_else(|| {
        anyhow::anyhow!(tr!(
            fr: "Impossible de lire RAM/vCPU dans `virsh dumpxml {vm_id}`",
            en: "Cannot read RAM/vCPU from `virsh dumpxml {vm_id}`",
        ))
    })
}

/// RAM convertie en MiB (attribut `unit`, KiB par défaut comme libvirt) et vCPU.
///
/// `config.xml` écrit par Virtum utilise `unit='MiB'`, `virsh dumpxml` renvoie `unit='KiB'`.
fn parse_ram_vcpu_from_xml(xml: &str) -> Option<(u32, u32)> {
    let (mem_attrs, mem_text) = xml_element(xml, "memory")?;
    let amount: u64 = mem_text.trim().parse().ok()?;
    let unit = xml_attribute_value(mem_attrs, "unit").unwrap_or_else(|| "KiB".to_string());
    let ram = u32::try_from(memory_to_mib(amount, &unit)?).ok()?;
    let (_, vcpu_text) = xml_element(xml, "vcpu")?;
    let vcpu = vcpu_text.trim().parse().ok()?;
    Some((ram, vcpu))
}

/// Attributs et texte du premier élément `<name …>texte</name>` (nom exact :
/// `<memory` ne correspond pas à `<memoryBacking`).
fn xml_element<'a>(xml: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{name}");
    let mut rest = xml;
    loop {
        let start = rest.find(&open)?;
        let after = &rest[start + open.len()..];
        if after.starts_with([' ', '>', '\t', '\n']) {
            let end_tag = after.find('>')?;
            let attrs = &after[..end_tag];
            let body = &after[end_tag + 1..];
            let text = &body[..body.find('<')?];
            return Some((attrs, text));
        }
        rest = after;
    }
}

/// Unités mémoire libvirt → MiB (arrondi inférieur).
fn memory_to_mib(amount: u64, unit: &str) -> Option<u64> {
    let bytes: u64 = match unit {
        "b" | "bytes" => 1,
        "KB" => 1_000,
        "k" | "KiB" => 1 << 10,
        "MB" => 1_000_000,
        "M" | "MiB" => 1 << 20,
        "GB" => 1_000_000_000,
        "G" | "GiB" => 1 << 30,
        "TB" => 1_000_000_000_000,
        "T" | "TiB" => 1 << 40,
        _ => return None,
    };
    Some(amount.checked_mul(bytes)? >> 20)
}

/// Réécrit `config.xml` avec RAM/CPU demandés et redéfinit le domaine libvirt (VM arrêtée).
pub fn redefine_domain_resources(
    instance_dir: &Path,
    vm_id: &str,
    ram_mb: u32,
    vcpus: u32,
    libvirt_network: Option<&str>,
    bridge_host_interface: Option<&str>,
) -> Result<()> {
    let xml_path = refresh_domain_xml_with_resources(
        instance_dir,
        vm_id,
        ram_mb,
        vcpus,
        libvirt_network,
        bridge_host_interface,
    )?;
    define_or_replace_domain(vm_id, &xml_path)
}

/// Comme [`refresh_domain_xml`] mais avec RAM/CPU explicites.
pub fn refresh_domain_xml_with_resources(
    instance_dir: &Path,
    vm_id: &str,
    ram_mb: u32,
    vcpus: u32,
    libvirt_network: Option<&str>,
    bridge_host_interface: Option<&str>,
) -> Result<PathBuf> {
    let xml_path = instance_dir.join("config.xml");
    let disk_path = instance_dir.join("disk.qcow2");
    let iso_path = instance_dir.join("cloud-init.iso");

    let nics = nics_from_refresh_args(libvirt_network, bridge_host_interface, &xml_path)?;
    let nic_mac = resolve_nic_mac(instance_dir, vm_id);
    let filesystems = filesystems_for_instance(instance_dir)?;
    let spec = DomainSpec {
        vm_id,
        ram_mb,
        vcpus,
        disk_path: &disk_path,
        iso_path: &iso_path,
        nics,
        nic_mac,
        filesystems,
    };
    write_domain_xml(instance_dir, &spec)
}

pub fn domain_exists(vm_id: &str) -> bool {
    run_virsh(&["dominfo", vm_id])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Noms de tous les domaines libvirt (actifs et arrêtés).
pub fn list_domain_names() -> Result<Vec<String>> {
    let output = run_virsh(&["list", "--all", "--name"])?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// Refuse d'agir sur un domaine qui porte le marqueur d'un autre Virtum (même nom de lab
/// chez un autre utilisateur du même libvirt). Un domaine sans marqueur reste accepté ici :
/// les appelants qui connaissent le dossier d'instance vérifient aussi ce cas
/// ([`crate::ownership::ensure_domain_ours`]).
fn refuse_foreign_domain(vm_id: &str) -> Result<()> {
    if let crate::ownership::Owner::Other(id) = crate::ownership::domain_owner(vm_id) {
        bail!(tr!(
            fr: "Le domaine {vm_id} appartient à un autre Virtum ({id}) : refus d'y toucher.",
            en: "The domain {vm_id} belongs to another Virtum ({id}): refusing to touch it.",
        ));
    }
    Ok(())
}

pub fn define_domain(xml_path: &Path) -> Result<()> {
    if let Some(vm_id) = parse_domain_name_from_xml(&std::fs::read_to_string(xml_path)?) {
        if domain_exists(&vm_id) {
            refuse_foreign_domain(&vm_id)?;
        }
    }
    run_virsh(&["define", path_arg(xml_path)?]).map(|_| ())
}

/// Nom du domaine (`<name>…</name>`) dans un XML libvirt.
fn parse_domain_name_from_xml(xml: &str) -> Option<String> {
    let start = xml.find("<name>")? + "<name>".len();
    let end = start + xml[start..].find("</name>")?;
    Some(xml[start..end].trim().to_string())
}

/// `virsh define` en remplaçant une définition existante (après `down`, `apply`, etc.).
pub fn define_or_replace_domain(vm_id: &str, xml_path: &Path) -> Result<()> {
    if domain_exists(vm_id) {
        destroy_domain(vm_id)?;
        undefine_domain(vm_id)?;
    }
    define_domain(xml_path)?;
    Ok(())
}

pub fn start_domain(vm_id: &str) -> Result<()> {
    if domain_exists(vm_id) {
        refuse_foreign_domain(vm_id)?;
        let state = domain_state(vm_id).unwrap_or_default();
        if state == "running" {
            bail!(tr!(
                fr: "Le domaine libvirt '{vm_id}' est déjà actif.",
                en: "The libvirt domain '{vm_id}' is already running.",
            ));
        }
    } else {
        bail!(tr!(
            fr: "Le domaine libvirt '{vm_id}' n'existe pas. \
             Exécutez virsh define avant virsh start.",
            en: "The libvirt domain '{vm_id}' does not exist. \
             Run virsh define before virsh start."
        ));
    }

    run_virsh(&["start", vm_id]).map(|_| ())
}

pub fn domain_state(vm_id: &str) -> Result<String> {
    let output = run_virsh(&["domstate", vm_id])?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Returns true for states where the guest is still running (ACPI shutdown needed).
///
/// `in shutdown` compte comme actif : QEMU tient encore le disque (verrou qcow2)
/// tant que l'état n'est pas `shut off`.
pub fn domain_is_active(state: &str) -> bool {
    matches!(
        state,
        "running" | "paused" | "pmsuspended" | "blocked" | "unknown" | "in shutdown"
    )
}

/// ACPI shutdown; no-op if the domain is undefined or already shut off.
pub fn shutdown_domain(vm_id: &str) -> Result<()> {
    if !domain_exists(vm_id) {
        return Ok(());
    }
    refuse_foreign_domain(vm_id)?;
    let state = domain_state(vm_id)?;
    // Déjà en cours d'arrêt : inutile de renvoyer l'ACPI.
    if state == "in shutdown" || !domain_is_active(&state) {
        return Ok(());
    }
    run_virsh(&["shutdown", vm_id]).map(|_| ())
}

/// Idempotent: succeeds if the domain is already stopped or undefined.
pub fn destroy_domain(vm_id: &str) -> Result<()> {
    if !domain_exists(vm_id) {
        return Ok(());
    }
    refuse_foreign_domain(vm_id)?;
    let _ = run_virsh(&["destroy", vm_id]);
    Ok(())
}

/// Démarrage du domaine au boot de l'hôte (`virsh autostart [--disable]`).
pub fn set_domain_autostart(vm_id: &str, enabled: bool) -> Result<()> {
    refuse_foreign_domain(vm_id)?;
    let args: &[&str] = if enabled {
        &["autostart", vm_id]
    } else {
        &["autostart", "--disable", vm_id]
    };
    run_virsh(args).map(|_| ()).with_context(|| {
        tr!(
            fr: "virsh autostart a échoué pour {vm_id}",
            en: "virsh autostart failed for {vm_id}",
        )
    })
}

/// Idempotent: succeeds if the domain is not defined.
pub fn undefine_domain(vm_id: &str) -> Result<()> {
    if !domain_exists(vm_id) {
        return Ok(());
    }
    refuse_foreign_domain(vm_id)?;
    run_virsh(&["undefine", vm_id]).map(|_| ())
}

pub fn domifaddr(vm_id: &str) -> Result<String> {
    let output = run_virsh(&["domifaddr", vm_id])?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Tries several `domifaddr` sources (lease, agent, default).
pub fn domifaddr_all_sources(vm_id: &str) -> Vec<String> {
    let variants: &[&[&str]] = &[
        &["domifaddr", vm_id],
        &["domifaddr", vm_id, "--source", "lease"],
        &["domifaddr", vm_id, "--source", "agent"],
        &["domifaddr", vm_id, "--source", "arp"],
    ];
    variants
        .iter()
        .filter_map(|args| run_virsh(args).ok())
        .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        .collect()
}

pub fn domiflist(vm_id: &str) -> Result<String> {
    let output = run_virsh(&["domiflist", vm_id])?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn net_dhcp_leases(network: &str) -> Result<String> {
    let output = run_virsh(&["net-dhcp-leases", network])?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn parse_ipv4_from_domifaddr(output: &str) -> Option<String> {
    select_best_guest_ipv4(parse_all_ipv4_from_domifaddr(output), false)
}

/// Toutes les IPv4 invitées trouvées dans la sortie `domifaddr`.
pub fn parse_all_ipv4_from_domifaddr(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| {
            !line.trim().is_empty() && !line.starts_with("---") && !line.starts_with(" Name")
        })
        .filter_map(extract_ipv4_token)
        .collect()
}

/// Meilleure IP pour le mode bridge (préfère le sous-réseau de la route par défaut hôte).
pub fn select_best_bridge_ipv4(ips: Vec<String>) -> Option<String> {
    select_best_guest_ipv4(ips, true)
}

/// Cherche une IPv4 voisine (ARP) pour le MAC de la VM (`ip neigh show`).
pub fn neighbor_ipv4_for_mac(mac: &str) -> Option<String> {
    use std::process::Command;
    let mac = mac.to_lowercase();
    let output = Command::new("ip").args(["neigh", "show"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let lower = line.to_lowercase();
        if !lower.contains(&mac) {
            continue;
        }
        if let Some(ip) = extract_ipv4_token(line) {
            return Some(ip);
        }
    }
    None
}

pub fn guest_network_ipv4_from_agent(vm_id: &str) -> Option<String> {
    let output = run_virsh(&[
        "qemu-agent-command",
        vm_id,
        r#"{"execute":"guest-network-get-interfaces"}"#,
    ])
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_guest_network_interfaces_json(stdout.trim())
}

pub fn parse_all_ipv4_from_guest_agent_json(raw: &str) -> Vec<String> {
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if value.get("error").is_some() {
        return Vec::new();
    }
    let interfaces = match value.get("return").and_then(|r| r.as_array()) {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut ips = Vec::new();
    for iface in interfaces {
        let name = iface.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if name == "lo" {
            continue;
        }
        let Some(addrs) = iface.get("ip-addresses").and_then(|a| a.as_array()) else {
            continue;
        };
        for addr in addrs {
            if addr.get("ip-address-type").and_then(|t| t.as_str()) != Some("ipv4") {
                continue;
            }
            // Réponse fournie par l'invité (root) : n'accepter qu'une IPv4 stricte, rendue
            // sous forme canonique (elle finit dans metadata.json et l'inventaire Ansible).
            if let Some(ip) = addr
                .get("ip-address")
                .and_then(|i| i.as_str())
                .and_then(|i| i.parse::<std::net::Ipv4Addr>().ok())
            {
                ips.push(ip.to_string());
            }
        }
    }
    ips
}

pub fn parse_guest_network_interfaces_json(raw: &str) -> Option<String> {
    select_best_guest_ipv4(parse_all_ipv4_from_guest_agent_json(raw), false)
}

pub fn guest_network_bridge_ipv4_from_agent(vm_id: &str) -> Option<String> {
    let output = run_virsh(&[
        "qemu-agent-command",
        vm_id,
        r#"{"execute":"guest-network-get-interfaces"}"#,
    ])
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    select_best_bridge_ipv4(parse_all_ipv4_from_guest_agent_json(stdout.trim()))
}

fn select_best_guest_ipv4(ips: Vec<String>, bridge_lan: bool) -> Option<String> {
    if bridge_lan {
        if let Some(host_prefix) = host_lan_prefix() {
            if let Some(ip) = ips
                .iter()
                .find(|ip| ip.starts_with(&host_prefix) && is_usable_guest_ipv4(ip))
            {
                return Some(ip.clone());
            }
        }
        return ips
            .into_iter()
            .find(|ip| is_usable_guest_ipv4(ip) && !ip.starts_with("192.168.122."));
    }
    ips.iter()
        .find(|ip| ip.starts_with("10.0.2.") || ip.starts_with("192.168."))
        .cloned()
        .or_else(|| ips.into_iter().find(|ip| is_usable_guest_ipv4(ip)))
}

/// Préfixe « a.b.c. » de l'IP source de la route par défaut (réseau actif du bureau / partage).
fn host_lan_prefix() -> Option<String> {
    use std::process::Command;
    let output = Command::new("ip")
        .args(["-4", "route", "show", "default"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if !line.trim().starts_with("default ") {
            continue;
        }
        if let Some(src_idx) = line.split_whitespace().position(|t| t == "src") {
            let parts: Vec<_> = line.split_whitespace().collect();
            if let Some(src) = parts.get(src_idx + 1) {
                return ipv4_prefix_three_octets(src);
            }
        }
    }
    None
}

fn ipv4_prefix_three_octets(ip: &str) -> Option<String> {
    let mut octets = ip.split('.');
    let a = octets.next()?;
    let b = octets.next()?;
    let c = octets.next()?;
    Some(format!("{a}.{b}.{c}."))
}

fn is_usable_guest_ipv4(ip: &str) -> bool {
    ip.parse::<std::net::Ipv4Addr>().is_ok()
        && !ip.starts_with("127.")
        && !ip.starts_with("169.254.")
        && ip != "0.0.0.0"
}

pub fn parse_mac_from_domiflist(output: &str) -> Option<String> {
    for line in output.lines().skip(2) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5 {
            return Some(parts[4].to_lowercase());
        }
    }
    None
}

/// Une ligne de `virsh net-dhcp-leases`, lue par colonnes fixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseLine {
    pub expiry: String,
    pub mac: String,
    pub ip: String,
}

fn is_mac_address(token: &str) -> bool {
    token.len() == 17
        && token.split(':').count() == 6
        && token
            .split(':')
            .all(|b| b.len() == 2 && b.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Lit une ligne de `virsh net-dhcp-leases` : date, heure, MAC, protocole, IP/préfixe,
/// puis nom d'hôte et client-id. Le nom d'hôte et le client-id sont choisis par
/// l'invité : seules les colonnes MAC et IP sont lues, à leur position.
pub fn parse_dhcp_lease_line(line: &str) -> Option<LeaseLine> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let [date, time, mac, proto, addr, ..] = tokens.as_slice() else {
        return None;
    };
    if !proto.eq_ignore_ascii_case("ipv4") || !is_mac_address(mac) {
        return None;
    }
    let (ip, prefix) = addr.split_once('/')?;
    ip.parse::<std::net::Ipv4Addr>().ok()?;
    prefix.parse::<u8>().ok().filter(|p| *p <= 32)?;
    Some(LeaseLine {
        expiry: format!("{date} {time}"),
        mac: mac.to_ascii_lowercase(),
        ip: ip.to_string(),
    })
}

pub fn parse_ipv4_from_dhcp_leases(output: &str, mac: &str) -> Option<String> {
    let mac = mac.to_ascii_lowercase();
    output
        .lines()
        .filter_map(parse_dhcp_lease_line)
        .find(|l| l.mac == mac)
        .map(|l| l.ip)
}

fn extract_ipv4_token(line: &str) -> Option<String> {
    for token in line.split_whitespace() {
        if token.contains('.') && token.contains('/') {
            let ip = token.split('/').next()?.trim();
            if ip.chars().filter(|c| *c == '.').count() == 3
                && ip.chars().all(|c| c.is_ascii_digit() || c == '.')
            {
                return Some(ip.to_string());
            }
        }
    }
    None
}

/// Chemin absolu résolu d'un socket virtiofsd. virtiofsd supprime son socket dès que QEMU
/// s'y connecte (et il n'existe pas non plus quand virtiofsd est arrêté) : seul le dossier
/// parent est résolu, le nom du fichier est ajouté tel quel.
fn socket_absolute_path(path: &Path) -> Result<String> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        bail!(tr!(
            fr: "Chemin de socket virtiofs invalide : {}",
            en: "Invalid virtiofs socket path: {}",
            path.display(),
        ));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let dir = std::fs::canonicalize(parent).with_context(|| {
        tr!(
            fr: "Impossible de résoudre le dossier du socket {}",
            en: "Cannot resolve the socket directory {}",
            path.display()
        )
    })?;
    Ok(dir.join(name).display().to_string())
}

fn absolute_path(path: &Path) -> Result<String> {
    let abs = std::fs::canonicalize(path).with_context(|| {
        tr!(
            fr: "Impossible de résoudre le chemin absolu de {}",
            en: "Cannot resolve the absolute path of {}",
            path.display()
        )
    })?;
    Ok(abs.display().to_string())
}

pub(crate) fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
        .replace('\t', "&#9;")
}

#[cfg(test)]
mod tests {
    /// Tout appel à virsh vise le libvirt système, explicitement : sans `-c`, virsh suivrait
    /// `LIBVIRT_DEFAULT_URI` et tomberait sur `qemu:///session` hors d'Ubuntu.
    #[test]
    fn virsh_always_targets_system_libvirt() {
        assert_eq!(
            virsh_args(&["net-info", "default"]),
            ["-c", "qemu:///system", "net-info", "default"]
        );
        let cmd = virsh_command();
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, ["-c", "qemu:///system"]);

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(src).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).unwrap();
            for (i, line) in text.lines().enumerate() {
                let raw_command = line.contains(concat!("Command::new(", "\"virsh\")"));
                let raw_run = line.contains(concat!("run_command(", "\"virsh\""))
                    && !line.contains("virsh_args");
                assert!(
                    !(raw_run || raw_command && name != "libvirt.rs"),
                    "{name}:{} appelle virsh sans -c qemu:///system : {line}",
                    i + 1
                );
            }
        }
    }

    use super::*;

    #[test]
    fn parses_ram_vcpu_from_virtum_config() {
        let xml = "<domain type='kvm'>\n  <name>vm-a</name>\n  <memory unit='MiB'>4096</memory>\n  <vcpu>2</vcpu>\n  <memoryBacking>\n    <source type='memfd'/>\n  </memoryBacking>\n</domain>";
        assert_eq!(parse_ram_vcpu_from_xml(xml), Some((4096, 2)));
    }

    #[test]
    fn parses_ram_vcpu_from_virsh_dumpxml() {
        // Forme renvoyée par `virsh dumpxml --inactive` : KiB, currentMemory, vcpu placement.
        let xml = r#"<domain type='kvm'>
  <name>vm-a</name>
  <uuid>0b0c</uuid>
  <memory unit='KiB'>2097152</memory>
  <currentMemory unit='KiB'>2097152</currentMemory>
  <memoryBacking>
    <source type='memfd'/>
  </memoryBacking>
  <vcpu placement='static'>4</vcpu>
</domain>"#;
        assert_eq!(parse_ram_vcpu_from_xml(xml), Some((2048, 4)));
    }

    #[test]
    fn memory_backing_before_memory_is_not_mistaken() {
        let xml = "<domain><memoryBacking><source type='memfd'/></memoryBacking><memory unit='GiB'>3</memory><vcpu>1</vcpu></domain>";
        assert_eq!(parse_ram_vcpu_from_xml(xml), Some((3072, 1)));
    }

    #[test]
    fn memory_without_unit_defaults_to_kib() {
        let xml = "<domain><memory>1048576</memory><vcpu>1</vcpu></domain>";
        assert_eq!(parse_ram_vcpu_from_xml(xml), Some((1024, 1)));
    }

    #[test]
    fn memory_unknown_unit_is_rejected() {
        let xml = "<domain><memory unit='parsec'>1</memory><vcpu>1</vcpu></domain>";
        assert_eq!(parse_ram_vcpu_from_xml(xml), None);
        assert_eq!(memory_to_mib(1, "GB"), Some(953));
    }

    #[test]
    fn domain_in_shutdown_still_active() {
        assert!(domain_is_active("in shutdown"));
        assert!(domain_is_active("running"));
        assert!(!domain_is_active("shut off"));
        assert!(!domain_is_active("crashed"));
    }

    #[test]
    fn domain_is_active_recognizes_running_and_shutoff() {
        assert!(domain_is_active("running"));
        assert!(domain_is_active("paused"));
        assert!(!domain_is_active("shut off"));
    }

    #[test]
    fn parses_domifaddr_ipv4() {
        let sample = r#" Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 vnet0      52:54:00:12:34:56    ipv4         192.168.122.45/24
"#;
        assert_eq!(
            parse_ipv4_from_domifaddr(sample).as_deref(),
            Some("192.168.122.45")
        );
    }

    #[test]
    fn parses_guest_agent_network_json() {
        let sample = r#"{"return":[{"name":"lo","ip-addresses":[{"ip-address-type":"ipv4","ip-address":"127.0.0.1","prefix":8}]},{"name":"eth0","ip-addresses":[{"ip-address-type":"ipv4","ip-address":"10.0.2.15","prefix":24}]}]}"#;
        assert_eq!(
            parse_guest_network_interfaces_json(sample).as_deref(),
            Some("10.0.2.15")
        );
    }

    #[test]
    fn skips_loopback_in_domifaddr() {
        let sample = r#" Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 lo         00:00:00:00:00:00    ipv4         127.0.0.1/8
 eth0       52:54:00:12:34:56    ipv4         10.0.2.15/24
"#;
        assert_eq!(
            parse_ipv4_from_domifaddr(sample).as_deref(),
            Some("10.0.2.15")
        );
    }

    #[test]
    fn parses_dhcp_lease_by_mac() {
        let sample = r#" Expiry Time          MAC address         Protocol    IP address           Hostname
-----------------------------------------------------------------------------------------
 0000-00-00 00:00:00   52:54:00:ab:cd:ef   ipv4        192.168.122.10/24   vm-test01
"#;
        assert_eq!(
            parse_ipv4_from_dhcp_leases(sample, "52:54:00:ab:cd:ef").as_deref(),
            Some("192.168.122.10")
        );
    }

    #[test]
    fn parse_mac_address_from_xml_ok() {
        let xml = r#"<interface type='network'>
      <mac address='52:54:00:12:34:56'/>
      <source network='virtum-lab-demo-lab'/>
    </interface>"#;
        assert_eq!(
            parse_mac_address_from_xml(xml).as_deref(),
            Some("52:54:00:12:34:56")
        );
    }

    #[test]
    fn generate_xml_embeds_mac() {
        let disk = std::env::temp_dir().join("virtum-xml-mac-disk.qcow2");
        let iso = std::env::temp_dir().join("virtum-xml-mac.iso");
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&iso, b"").unwrap();
        let spec = DomainSpec {
            vm_id: "vm-mac",
            ram_mb: 512,
            vcpus: 1,
            disk_path: &disk,
            iso_path: &iso,
            nics: vec![VmNic::LibvirtNetwork {
                name: "virtum-lab-x".into(),
            }],
            nic_mac: Some("52:54:00:ab:cd:ef".into()),
            filesystems: Vec::new(),
        };
        let xml = generate_domain_xml(&spec).unwrap();
        assert!(xml.contains("52:54:00:ab:cd:ef"));
        let _ = std::fs::remove_file(disk);
        let _ = std::fs::remove_file(iso);
    }

    #[test]
    fn parses_network_from_domain_xml() {
        let xml = r#"<interface type='network'>
      <source network='virtum-lab-demo-lab'/>
    </interface>"#;
        assert_eq!(
            parse_network_from_xml(xml).as_deref(),
            Some("virtum-lab-demo-lab")
        );
    }

    #[test]
    fn generate_xml_contains_paths_and_headless_graphics() {
        let disk = std::env::temp_dir().join("virtum-xml-disk.qcow2");
        let iso = std::env::temp_dir().join("virtum-xml.iso");
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&iso, b"").unwrap();

        let spec = DomainSpec::with_libvirt_network(
            "vm-testxml",
            2048,
            2,
            &disk,
            &iso,
            "virtum-lab-demo-lab",
        );
        let xml = generate_domain_xml(&spec).unwrap();
        assert!(xml.contains("<name>vm-testxml</name>"));
        assert!(xml.contains("network='virtum-lab-demo-lab'"));
        assert!(!xml.contains("<graphics"));
        assert!(xml.contains("type='network'"));
        assert!(!xml.contains("qemu:commandline"));

        let _ = std::fs::remove_file(disk);
        let _ = std::fs::remove_file(iso);
    }

    #[test]
    fn select_best_bridge_ipv4_prefers_host_subnet() {
        let ips = vec!["192.168.122.10".to_string(), "192.168.43.55".to_string()];
        // Sans route hôte en test, prend la première IP hors 192.168.122.
        let ip = select_best_bridge_ipv4(ips).unwrap();
        assert_eq!(ip, "192.168.43.55");
    }

    #[test]
    fn generate_xml_dual_nic_lab_and_bridge() {
        let dir = std::env::temp_dir().join(format!("virtum-dual-nic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.qcow2");
        let iso = dir.join("cloud-init.iso");
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&iso, b"").unwrap();
        let spec = DomainSpec {
            vm_id: "vm-dual",
            ram_mb: 1024,
            vcpus: 1,
            disk_path: &disk,
            iso_path: &iso,
            nics: vec![
                VmNic::LibvirtNetwork {
                    name: "virtum-lab-demo".into(),
                },
                VmNic::MacvtapBridge {
                    host_interface: "enp3s0".into(),
                },
            ],
            nic_mac: Some("52:54:00:ab:cd:ef".into()),
            filesystems: Vec::new(),
        };
        let xml = generate_domain_xml(&spec).unwrap();
        assert!(xml.contains("network='virtum-lab-demo'"));
        assert!(xml.contains("mode='bridge'"));
        assert!(xml.contains("52:54:00:ab:cd:ef"));
    }

    #[test]
    fn generate_xml_macvtap_bridge() {
        let disk = std::env::temp_dir().join("virtum-xml-br-disk.qcow2");
        let iso = std::env::temp_dir().join("virtum-xml-br.iso");
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&iso, b"").unwrap();

        let spec = DomainSpec {
            vm_id: "vm-bridge",
            ram_mb: 2048,
            vcpus: 2,
            disk_path: &disk,
            iso_path: &iso,
            nics: vec![VmNic::MacvtapBridge {
                host_interface: "enp3s0".to_string(),
            }],
            nic_mac: None,
            filesystems: Vec::new(),
        };
        let xml = generate_domain_xml(&spec).unwrap();
        assert!(xml.contains("type='direct'"));
        assert!(xml.contains("dev='enp3s0'"));
        assert!(xml.contains("mode='bridge'"));

        let parsed = parse_nics_from_xml(&xml);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0],
            VmNic::MacvtapBridge {
                host_interface: "enp3s0".to_string()
            }
        );

        let _ = std::fs::remove_file(disk);
        let _ = std::fs::remove_file(iso);
    }

    #[test]
    fn generate_xml_virtiofs_includes_shared_memory_backing() {
        let dir = std::env::temp_dir().join(format!("virtum-fs-xml-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.qcow2");
        let iso = dir.join("cloud-init.iso");
        let sock = dir.join("virtiofs-app.sock");
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&iso, b"").unwrap();
        // Pas de fichier socket : virtiofsd le supprime dès que QEMU s'y connecte.
        let spec = DomainSpec {
            vm_id: "vm-fs",
            ram_mb: 1024,
            vcpus: 1,
            disk_path: &disk,
            iso_path: &iso,
            nics: vec![VmNic::LibvirtNetwork {
                name: "default".into(),
            }],
            nic_mac: None,
            filesystems: vec![VirtiofsSpec {
                tag: "app".into(),
                socket_path: sock,
            }],
        };
        let xml = generate_domain_xml(&spec).unwrap();
        assert!(xml.contains("type='virtiofs'"));
        let expected_sock = dir.canonicalize().unwrap().join("virtiofs-app.sock");
        assert!(xml.contains(&format!("<source socket='{}'/>", expected_sock.display())));
        assert!(xml.contains("<access mode='shared'/>"));
        assert!(xml.contains("<source type='memfd'/>"));
        assert!(xml.contains("seclabel type='none'"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Audit : une adresse fournie par l'agent invité qui n'est pas une IPv4 stricte est
    /// ignorée (elle finirait dans l'inventaire Ansible).
    #[test]
    fn guest_agent_ip_must_be_strict_ipv4() {
        let raw = r#"{"return":[{"name":"eth1","ip-addresses":[
            {"ip-address-type":"ipv4","ip-address":"192.168.1.50' ansible_connection='local"},
            {"ip-address-type":"ipv4","ip-address":"192.168.1.51"}]}]}"#;
        assert_eq!(
            parse_all_ipv4_from_guest_agent_json(raw),
            vec!["192.168.1.51"]
        );
        assert!(!is_usable_guest_ipv4("10.0.0.1 x"));
    }

    #[test]
    fn domain_name_is_read_from_xml() {
        let xml =
            "<domain type='kvm'>\n  <name>vm-0123456789ab</name>\n  <memory>1</memory>\n</domain>";
        assert_eq!(
            parse_domain_name_from_xml(xml).as_deref(),
            Some("vm-0123456789ab")
        );
        assert_eq!(parse_domain_name_from_xml("<domain/>"), None);
    }

    #[test]
    fn xml_escape_encodes_line_breaks() {
        assert_eq!(xml_escape("a\nb\r\tc"), "a&#10;b&#13;&#9;c");
    }
}
