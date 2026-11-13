//! Inventaire Ansible généré depuis un lab (`virtum inventory`, v0.8).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::cli::InventoryFormat;
use crate::config::{self, VirtumPaths};
use crate::libvirt::{domain_exists, domain_is_active, domain_state};
use crate::metadata::{self, Instance, Metadata};
use crate::ssh::{self, SshSessionOpts};
use crate::virtumfile::{self, Virtumfile};

/// Hôte de l'inventaire : nom de machine + variables d'hôte (ordre stable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryHost {
    pub name: String,
    pub vars: Vec<(String, String)>,
}

/// Inventaire d'un lab : groupe implicite (tous les hôtes, avec leurs variables)
/// et groupes déclarés (`groups:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub lab_name: String,
    pub lab_group: String,
    pub hosts: Vec<InventoryHost>,
    pub groups: BTreeMap<String, Vec<String>>,
}

/// Construit l'inventaire. `is_running` indique si la VM tourne (libvirt en vrai,
/// valeur fixe en test). Renvoie aussi les avertissements (machines exclues).
pub fn build(
    lab: &Virtumfile,
    metadata: &Metadata,
    paths: &VirtumPaths,
    insecure_ssh: bool,
    is_running: impl Fn(&Instance) -> bool,
) -> (Inventory, Vec<String>) {
    let mut warnings = Vec::new();
    let mut hosts = Vec::new();
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let private_key = ssh::private_key_path(&paths.keys);

    for (machine, spec) in &lab.machines {
        let Some(inst) = metadata::find_by_lab_machine(metadata, &lab.name, machine) else {
            warnings.push(format!(
                "{machine} non créée : exclue de l'inventaire — virtum up"
            ));
            continue;
        };
        if !is_running(inst) {
            warnings.push(format!(
                "{machine} arrêtée : exclue de l'inventaire — virtum up"
            ));
            continue;
        }
        let Some(ip) = inst.ip.clone() else {
            warnings.push(format!(
                "{machine} sans adresse IP connue : exclue de l'inventaire"
            ));
            continue;
        };
        let opts = SshSessionOpts::new(&paths.instances.join(&inst.id), insecure_ssh);
        let (strict, known_hosts) = ssh::ssh_option_strings(&opts);
        let port = inst.ssh_port.unwrap_or(virtumfile::DEFAULT_SSH_PORT);
        let mut vars = vec![
            ("ansible_host".to_string(), ip),
            ("ansible_port".to_string(), port.to_string()),
            ("ansible_user".to_string(), inst.user.clone()),
            (
                "ansible_ssh_private_key_file".to_string(),
                private_key.display().to_string(),
            ),
            (
                "ansible_ssh_common_args".to_string(),
                format!("-F /dev/null -o IdentitiesOnly=yes -o {strict} -o {known_hosts}"),
            ),
            ("virtum_vm_id".to_string(), inst.id.clone()),
            ("virtum_hostname".to_string(), inst.hostname.clone()),
        ];
        if let Some(lan) = inst.lan_ip.clone() {
            vars.push(("virtum_lan_ip".to_string(), lan));
        }
        hosts.push(InventoryHost {
            name: machine.clone(),
            vars,
        });
        for group in &spec.groups {
            groups
                .entry(group.clone())
                .or_default()
                .push(machine.clone());
        }
    }

    (
        Inventory {
            lab_name: lab.name.clone(),
            lab_group: virtumfile::lab_group_name(&lab.name),
            hosts,
            groups,
        },
        warnings,
    )
}

/// Valeur INI d'une variable d'hôte (Ansible découpe la ligne avec shlex).
///
/// Une valeur ne doit jamais pouvoir ajouter une autre variable (`x' ansible_connection='local`)
/// ni couper la ligne : les caractères de contrôle sont remplacés, et une valeur contenant
/// une apostrophe est mise entre guillemets doubles avec échappement.
fn ini_value(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let needs_quotes = value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '#' | ';'));
    if !needs_quotes {
        return value;
    }
    if !value.contains('\'') {
        return format!("'{value}'");
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub fn render_ini(inv: &Inventory) -> String {
    let mut out = format!(
        "# Généré par virtum inventory — lab {} (ne pas éditer)\n\
         # Groupe implicite du lab : chaque hôte y est déclaré avec ses variables.\n[{}]\n",
        inv.lab_name, inv.lab_group
    );
    for host in &inv.hosts {
        out.push_str(&host.name);
        for (key, value) in &host.vars {
            out.push_str(&format!(" {key}={}", ini_value(value)));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "\n[{}:vars]\nvirtum_lab={}\n",
        inv.lab_group, inv.lab_name
    ));
    for (group, members) in &inv.groups {
        if *group == inv.lab_group {
            continue;
        }
        out.push_str(&format!("\n[{group}]\n"));
        for m in members {
            out.push_str(m);
            out.push('\n');
        }
    }
    out
}

pub fn render_yaml(inv: &Inventory) -> Result<String> {
    use serde_yaml::{Mapping, Value};
    let s = |v: &str| Value::String(v.to_string());

    let mut lab_hosts = Mapping::new();
    for host in &inv.hosts {
        let mut vars = Mapping::new();
        for (key, value) in &host.vars {
            let v = if key == "ansible_port" {
                value
                    .parse::<u64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| s(value))
            } else {
                s(value)
            };
            vars.insert(s(key), v);
        }
        lab_hosts.insert(s(&host.name), Value::Mapping(vars));
    }
    let mut lab_vars = Mapping::new();
    lab_vars.insert(s("virtum_lab"), s(&inv.lab_name));
    let mut lab_group = Mapping::new();
    lab_group.insert(s("hosts"), Value::Mapping(lab_hosts));
    lab_group.insert(s("vars"), Value::Mapping(lab_vars));

    let mut children = Mapping::new();
    children.insert(s(&inv.lab_group), Value::Mapping(lab_group));
    for (group, members) in &inv.groups {
        if *group == inv.lab_group {
            continue;
        }
        let mut hosts = Mapping::new();
        for m in members {
            hosts.insert(s(m), Value::Null);
        }
        let mut g = Mapping::new();
        g.insert(s("hosts"), Value::Mapping(hosts));
        children.insert(s(group), Value::Mapping(g));
    }
    let mut all = Mapping::new();
    all.insert(s("children"), Value::Mapping(children));
    let mut root = Mapping::new();
    root.insert(s("all"), Value::Mapping(all));

    let body =
        serde_yaml::to_string(&Value::Mapping(root)).context("Rendu YAML de l'inventaire")?;
    Ok(format!(
        "# Généré par virtum inventory — lab {} (ne pas éditer)\n{body}",
        inv.lab_name
    ))
}

fn is_domain_running(inst: &Instance) -> bool {
    domain_exists(&inst.id)
        && domain_state(&inst.id)
            .map(|s| domain_is_active(&s))
            .unwrap_or(false)
}

/// Inventaire du lab avec l'état réel des VM (libvirt) ; erreur si aucun hôte.
pub async fn lab_inventory(
    lab: &Virtumfile,
    paths: &VirtumPaths,
    insecure_ssh: bool,
) -> Result<(Inventory, Vec<String>)> {
    let metadata = metadata::load_metadata(&paths.metadata).await?;
    let (inv, warnings) = build(lab, &metadata, paths, insecure_ssh, is_domain_running);
    if inv.hosts.is_empty() {
        for w in &warnings {
            eprintln!("Avertissement: {w}");
        }
        bail!(
            "Aucune machine du lab '{}' ne tourne : inventaire vide. Lancez `virtum up`.",
            lab.name
        );
    }
    Ok((inv, warnings))
}

/// Écrit un fichier de façon atomique (temporaire + renommage), droits 0644.
pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    crate::fsutil::write_atomic(path, content.as_bytes(), 0o644)
}

pub fn inventory(
    file: Option<PathBuf>,
    format: InventoryFormat,
    output: Option<PathBuf>,
    insecure_ssh: bool,
) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(async {
            let path = virtumfile::resolve_virtumfile_path(file)?;
            let lab = virtumfile::load(&path)?;
            let paths = config::ensure_layout().await?;
            let (inv, warnings) = lab_inventory(&lab, &paths, insecure_ssh).await?;
            for w in &warnings {
                eprintln!("Avertissement: {w}");
            }
            let text = match format {
                InventoryFormat::Ini => render_ini(&inv),
                InventoryFormat::Yaml => render_yaml(&inv)?,
            };
            match output {
                Some(out) => {
                    write_atomic(&out, &text)?;
                    eprintln!(
                        "Inventaire écrit : {} ({} hôte(s))",
                        out.display(),
                        inv.hosts.len()
                    );
                }
                None => print!("{text}"),
            }
            Ok(())
        })
}

#[cfg(test)]
mod tests {
    /// Audit : une valeur ne peut ni fermer sa citation ni couper la ligne.
    #[test]
    fn ini_value_cannot_break_out() {
        assert_eq!(ini_value("192.168.1.5"), "192.168.1.5");
        assert_eq!(ini_value("a b"), "'a b'");
        assert_eq!(
            ini_value("10.9.9.9' ansible_connection='local"),
            "\"10.9.9.9' ansible_connection='local\""
        );
        assert_eq!(
            ini_value("x\nansible_connection=local"),
            "'x ansible_connection=local'"
        );
        assert_eq!(ini_value(r#"a"b'c"#), r#""a\"b'c""#);
    }

    use super::*;

    fn fixture() -> (Virtumfile, Metadata, VirtumPaths) {
        let lab = virtumfile::load_from_str(
            "name: kubernetes-lab\nmachines:\n  control-plane:\n    os: ubuntu-24.04\n    groups: [control_plane, k8s_cluster]\n  node-1:\n    os: ubuntu-24.04\n    groups: [workers, k8s_cluster]\n  node-2:\n    os: ubuntu-24.04\n  ghost:\n    os: ubuntu-24.04\n",
        )
        .unwrap();
        let inst = |id: &str, machine: &str, ip: &str, status: &str| -> Instance {
            serde_json::from_str(&format!(
                r#"{{"id":"{id}","name":"{machine}","os":"ubuntu-24.04","status":"{status}",
                    "ip":"{ip}","user":"virtum","hostname":"kubernetes-lab-{machine}",
                    "lab_name":"kubernetes-lab","machine_name":"{machine}",
                    "created_at":"2026-11-20T09:00:00Z"}}"#
            ))
            .unwrap()
        };
        let mut cp = inst("vm-cp", "control-plane", "192.168.218.10", "running");
        cp.ssh_port = Some(2222);
        let metadata = Metadata {
            instances: vec![
                cp,
                inst("vm-n1", "node-1", "192.168.218.11", "running"),
                inst("vm-n2", "node-2", "192.168.218.12", "stopped"),
            ],
            ..Default::default()
        };
        let root = PathBuf::from("/var/lib/virtum");
        let paths = VirtumPaths {
            root: root.clone(),
            images: root.join("images"),
            instances: root.join("instances"),
            keys: root.join("keys"),
            metadata: root.join("metadata.json"),
        };
        (lab, metadata, paths)
    }

    #[test]
    fn builds_hosts_groups_and_exclusions() {
        let (lab, metadata, paths) = fixture();
        let (inv, warnings) = build(&lab, &metadata, &paths, false, |i| {
            i.status == metadata::InstanceStatus::Running
        });
        let names: Vec<_> = inv.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["control-plane", "node-1"]);
        assert_eq!(inv.lab_group, "kubernetes_lab");
        assert_eq!(inv.groups["k8s_cluster"], vec!["control-plane", "node-1"]);
        assert_eq!(inv.groups["workers"], vec!["node-1"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.starts_with("ghost non créée")));
        assert!(warnings.iter().any(|w| w.starts_with("node-2 arrêtée")));

        let cp = &inv.hosts[0];
        let var = |k: &str| cp.vars.iter().find(|(key, _)| key == k).unwrap().1.clone();
        assert_eq!(var("ansible_host"), "192.168.218.10");
        assert_eq!(var("ansible_port"), "2222");
        assert_eq!(
            var("ansible_ssh_private_key_file"),
            "/var/lib/virtum/keys/virtum_id_ed25519"
        );
        assert_eq!(
            var("ansible_ssh_common_args"),
            "-F /dev/null -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new \
             -o UserKnownHostsFile=/var/lib/virtum/instances/vm-cp/known_hosts"
        );
        assert_eq!(var("virtum_hostname"), "kubernetes-lab-control-plane");

        let (insecure, _) = build(&lab, &metadata, &paths, true, |_| true);
        assert!(insecure.hosts[0]
            .vars
            .iter()
            .any(|(k, v)| k == "ansible_ssh_common_args"
                && v.contains("StrictHostKeyChecking=no")
                && v.contains("UserKnownHostsFile=/dev/null")));
    }

    #[test]
    fn renders_ini() {
        let (lab, metadata, paths) = fixture();
        let (inv, _) = build(&lab, &metadata, &paths, false, |i| {
            i.status == metadata::InstanceStatus::Running
        });
        let ini = render_ini(&inv);
        assert!(ini.contains(
            "\n[kubernetes_lab]\ncontrol-plane ansible_host=192.168.218.10 ansible_port=2222 "
        ));
        assert!(ini.contains(
            "ansible_ssh_common_args='-F /dev/null -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/var/lib/virtum/instances/vm-cp/known_hosts'"
        ));
        assert!(ini.contains("\n[kubernetes_lab:vars]\nvirtum_lab=kubernetes-lab\n"));
        assert!(ini.contains("\n[control_plane]\ncontrol-plane\n"));
        assert!(ini.contains("\n[k8s_cluster]\ncontrol-plane\nnode-1\n"));
        assert!(!ini.contains("node-2"));
    }

    #[test]
    fn renders_yaml() {
        let (lab, metadata, paths) = fixture();
        let (inv, _) = build(&lab, &metadata, &paths, false, |i| {
            i.status == metadata::InstanceStatus::Running
        });
        let yaml = render_yaml(&inv).unwrap();
        let body: serde_yaml::Value =
            serde_yaml::from_str(yaml.lines().skip(1).collect::<Vec<_>>().join("\n").as_str())
                .unwrap();
        let children = &body["all"]["children"];
        assert_eq!(
            children["kubernetes_lab"]["hosts"]["control-plane"]["ansible_port"],
            serde_yaml::Value::from(2222u64)
        );
        assert_eq!(
            children["kubernetes_lab"]["vars"]["virtum_lab"],
            serde_yaml::Value::from("kubernetes-lab")
        );
        assert!(children["workers"]["hosts"]
            .as_mapping()
            .unwrap()
            .contains_key("node-1"));
    }
}
