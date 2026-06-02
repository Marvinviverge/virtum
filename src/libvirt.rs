//! libvirt domain XML and virsh wrappers (steps 13–14).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::command::{path_arg, run_command};

/// Réseau NAT libvirt v0.1 (typiquement 192.168.122.0/24).
pub const LIBVIRT_DEFAULT_NETWORK: &str = "default";

/// Ancienne IP SLIRP (instances créées avant le réseau `default`).
pub const SLIRP_GUEST_IPV4: &str = "10.0.2.15";

fn run_virsh(args: &[&str]) -> Result<std::process::Output> {
    run_command("virsh", args)
}

const QEMU_MACHINE: &str = "pc-q35-7.2";

pub struct DomainSpec<'a> {
    pub vm_id: &'a str,
    pub ram_mb: u32,
    pub vcpus: u32,
    pub disk_path: &'a Path,
    pub iso_path: &'a Path,
}

/// Builds libvirt domain XML for a headless KVM guest on the `default` NAT network.
pub fn generate_domain_xml(spec: &DomainSpec<'_>) -> Result<String> {
    let disk = absolute_path(spec.disk_path)?;
    let iso = absolute_path(spec.iso_path)?;

    Ok(format!(
        r#"<domain type='kvm'>
  <name>{name}</name>
  <memory unit='MiB'>{ram}</memory>
  <vcpu>{vcpus}</vcpu>
  <os>
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
    <interface type='network'>
      <source network='{network}'/>
      <model type='virtio'/>
    </interface>
    <console type='pty'>
      <target type='serial' port='0'/>
    </console>
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
        machine = QEMU_MACHINE,
        disk = xml_escape(&disk),
        iso = xml_escape(&iso),
        network = LIBVIRT_DEFAULT_NETWORK,
    ))
}

pub fn write_domain_xml(instance_dir: &Path, spec: &DomainSpec<'_>) -> Result<PathBuf> {
    let xml = generate_domain_xml(spec)?;
    let xml_path = instance_dir.join("config.xml");
    std::fs::write(&xml_path, xml)
        .with_context(|| format!("Impossible d'écrire {}", xml_path.display()))?;
    Ok(xml_path)
}

pub fn domain_exists(vm_id: &str) -> bool {
    run_virsh(&["dominfo", vm_id])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn define_domain(xml_path: &Path) -> Result<()> {
    run_virsh(&["define", path_arg(xml_path)?]).map(|_| ())
}

pub fn start_domain(vm_id: &str) -> Result<()> {
    if domain_exists(vm_id) {
        let state = domain_state(vm_id).unwrap_or_default();
        if state == "running" {
            bail!("Le domaine libvirt '{vm_id}' est déjà actif.");
        }
    } else {
        bail!(
            "Le domaine libvirt '{vm_id}' n'existe pas. \
             Exécutez virsh define avant virsh start."
        );
    }

    run_virsh(&["start", vm_id]).map(|_| ())
}

pub fn domain_state(vm_id: &str) -> Result<String> {
    let output = run_virsh(&["domstate", vm_id])?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Returns true for states where the guest is still running (ACPI shutdown needed).
pub fn domain_is_active(state: &str) -> bool {
    matches!(
        state,
        "running" | "paused" | "pmsuspended" | "blocked" | "unknown"
    )
}

/// ACPI shutdown; no-op if the domain is undefined or already shut off.
pub fn shutdown_domain(vm_id: &str) -> Result<()> {
    if !domain_exists(vm_id) {
        return Ok(());
    }
    let state = domain_state(vm_id)?;
    if state == "shut off" || !domain_is_active(&state) {
        return Ok(());
    }
    run_virsh(&["shutdown", vm_id]).map(|_| ())
}

/// Idempotent: succeeds if the domain is already stopped or undefined.
pub fn destroy_domain(vm_id: &str) -> Result<()> {
    if !domain_exists(vm_id) {
        return Ok(());
    }
    let _ = run_virsh(&["destroy", vm_id]);
    Ok(())
}

/// Idempotent: succeeds if the domain is not defined.
pub fn undefine_domain(vm_id: &str) -> Result<()> {
    if !domain_exists(vm_id) {
        return Ok(());
    }
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
    let ips: Vec<String> = output
        .lines()
        .filter(|line| {
            !line.trim().is_empty() && !line.starts_with("---") && !line.starts_with(" Name")
        })
        .filter_map(extract_ipv4_token)
        .collect();
    select_best_guest_ipv4(ips)
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

pub fn parse_guest_network_interfaces_json(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("error").is_some() {
        return None;
    }
    let interfaces = value.get("return")?.as_array()?;
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
            if let Some(ip) = addr.get("ip-address").and_then(|i| i.as_str()) {
                ips.push(ip.to_string());
            }
        }
    }
    select_best_guest_ipv4(ips)
}

fn select_best_guest_ipv4(ips: Vec<String>) -> Option<String> {
    ips.iter()
        .find(|ip| ip.starts_with("10.0.2.") || ip.starts_with("192.168."))
        .cloned()
        .or_else(|| ips.into_iter().find(|ip| is_usable_guest_ipv4(ip)))
}

fn is_usable_guest_ipv4(ip: &str) -> bool {
    !ip.starts_with("127.") && !ip.starts_with("169.254.") && ip != "0.0.0.0"
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

pub fn parse_ipv4_from_dhcp_leases(output: &str, mac: &str) -> Option<String> {
    let mac = mac.to_lowercase();
    for line in output.lines() {
        let lower = line.to_lowercase();
        if lower.contains(&mac) {
            if let Some(ip) = extract_ipv4_token(line) {
                return Some(ip);
            }
        }
    }
    None
}

pub fn parse_ipv4_from_dhcp_leases_by_hostname(output: &str, hostname: &str) -> Option<String> {
    for line in output.lines() {
        if line.contains(hostname) {
            if let Some(ip) = extract_ipv4_token(line) {
                return Some(ip);
            }
        }
    }
    None
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

fn absolute_path(path: &Path) -> Result<String> {
    let abs = std::fs::canonicalize(path).with_context(|| {
        format!(
            "Impossible de résoudre le chemin absolu de {}",
            path.display()
        )
    })?;
    Ok(abs.display().to_string())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn generate_xml_contains_paths_and_headless_graphics() {
        let disk = std::env::temp_dir().join("virtum-xml-disk.qcow2");
        let iso = std::env::temp_dir().join("virtum-xml.iso");
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&iso, b"").unwrap();

        let spec = DomainSpec {
            vm_id: "vm-testxml",
            ram_mb: 2048,
            vcpus: 2,
            disk_path: &disk,
            iso_path: &iso,
        };
        let xml = generate_domain_xml(&spec).unwrap();
        assert!(xml.contains("<name>vm-testxml</name>"));
        assert!(!xml.contains("<graphics"));
        assert!(xml.contains("type='network'"));
        assert!(xml.contains("network='default'"));

        let _ = std::fs::remove_file(disk);
        let _ = std::fs::remove_file(iso);
    }
}
