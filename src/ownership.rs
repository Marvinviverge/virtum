//! Marqueur de propriétaire Virtum dans les objets libvirt (domaines, réseaux de lab).
//!
//! Sur `qemu:///system`, l'espace de noms libvirt est partagé entre utilisateurs alors que
//! l'état Virtum (metadata.json) est propre à chaque racine d'état. Les noms des VM de lab
//! (`vm-<sha256(lab:machine)>`) et des réseaux (`virtum-lab-<lab>`) sont déterministes : deux
//! utilisateurs avec le même lab produisent les mêmes noms. Virtum n'arrête, ne supprime ni
//! ne recrée un objet libvirt qu'après avoir vérifié qu'il porte son marqueur.

use std::path::Path;

use crate::fsutil::current_uid;
use crate::libvirt::xml_escape;
use crate::tr;

/// Espace de noms XML du marqueur (`<metadata>` libvirt).
pub const OWNER_NS: &str = "https://virtum.dev/xmlns/owner/1";

/// Identité de ce Virtum : uid + racine d'état (canonique si possible).
pub fn owner_tag() -> String {
    let root = crate::config::virtum_home()
        .map(|p| p.canonicalize().unwrap_or(p))
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!("{}:{root}", current_uid())
}

/// Bloc `<metadata>` à insérer dans le XML d'un domaine ou d'un réseau.
pub fn metadata_xml(indent: &str) -> String {
    format!(
        "{indent}<metadata>\n{indent}  <virtum:owner xmlns:virtum='{OWNER_NS}' id='{}'/>\n{indent}</metadata>\n",
        xml_escape(&owner_tag())
    )
}

/// Propriétaire lu dans un élément `<virtum:owner … id='…'/>`.
pub fn parse_owner_id(xml: &str) -> Option<String> {
    // `virsh metadata` renvoie l'élément sans préfixe (`<owner id="…"/>`) ; le XML du
    // domaine le porte avec préfixe (`<virtum:owner … id='…'/>`).
    let start = xml.find("<owner ").or_else(|| xml.find(":owner "))?;
    let after = xml[start..].split(" id=").nth(1)?;
    let quote = after.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let value = after[1..].split(quote).next()?;
    Some(
        value
            .replace("&apos;", "'")
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&#10;", "\n")
            .replace("&#13;", "\r")
            .replace("&#9;", "\t")
            .replace("&amp;", "&"),
    )
}

fn virsh_metadata(cmd: &str, object: &str) -> Option<String> {
    let output = crate::libvirt::virsh_command()
        .args([cmd, object, OWNER_NS])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_owner_id(&String::from_utf8_lossy(&output.stdout))
}

/// Ownership of a libvirt object, from the caller's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// Marqueur présent et identique à ce Virtum.
    Us,
    /// Marqueur présent mais d'un autre utilisateur / d'une autre racine d'état.
    Other(String),
    /// Aucun marqueur (objet créé avant ce correctif ou hors Virtum).
    Unknown,
}

fn classify(found: Option<String>) -> Owner {
    match found {
        Some(id) if id == owner_tag() => Owner::Us,
        Some(id) => Owner::Other(id),
        None => Owner::Unknown,
    }
}

/// Propriétaire d'un domaine libvirt.
pub fn domain_owner(vm_id: &str) -> Owner {
    classify(virsh_metadata("metadata", vm_id))
}

/// Propriétaire d'un réseau libvirt.
pub fn network_owner(net_name: &str) -> Owner {
    classify(virsh_metadata("net-metadata", net_name))
}

/// Domaines libvirt (actifs ou non) qui ont une interface sur ce réseau. `None` si la
/// liste n'a pas pu être établie (à traiter comme « réseau utilisé »).
fn network_attached_domains(net_name: &str) -> Option<Vec<String>> {
    let names = crate::libvirt::list_domain_names().ok()?;
    let mut attached = Vec::new();
    for dom in names {
        let output = crate::libvirt::virsh_command()
            .args(["domiflist", &dom])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        if String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|tok| tok == net_name)
        {
            attached.push(dom);
        }
    }
    Some(attached)
}

/// Au moins un domaine libvirt (actif ou non) a une interface sur ce réseau.
pub fn network_has_attached_domains(net_name: &str) -> bool {
    network_attached_domains(net_name).is_none_or(|d| !d.is_empty())
}

/// Virtum peut réutiliser ou modifier ce réseau (réservations DHCP) : marqué par ce
/// Virtum, ou non marqué (créé avant le marqueur) et dont chaque domaine attaché est à
/// cet état (marqué par lui, ou non marqué et utilisant le disque d'une de ses instances).
/// Un domaine non marqué d'un autre compte suffit à refuser.
pub fn network_may_modify(net_name: &str) -> bool {
    match network_owner(net_name) {
        Owner::Us => true,
        Owner::Other(_) => false,
        Owner::Unknown => {
            let Ok(home) = crate::config::virtum_home() else {
                return false;
            };
            let instances = home.join("instances");
            network_attached_domains(net_name).is_some_and(|doms| {
                doms.iter()
                    .all(|d| domain_is_ours_for_instance(d, &instances.join(d)))
            })
        }
    }
}

/// Virtum peut détruire ce réseau : marqué par ce Virtum, ou non marqué sans aucun
/// domaine attaché.
pub fn network_may_destroy(net_name: &str) -> bool {
    match network_owner(net_name) {
        Owner::Us => true,
        Owner::Other(_) => false,
        Owner::Unknown => !network_has_attached_domains(net_name),
    }
}

/// `true` si le XML de domaine utilise `disk` comme fichier source d'un disque.
fn xml_uses_disk(xml: &str, disk: &Path) -> bool {
    let disk = xml_escape(&disk.display().to_string());
    [format!("file='{disk}'"), format!("file=\"{disk}\"")]
        .iter()
        .any(|needle| xml.contains(needle.as_str()))
}

/// Virtum peut arrêter et supprimer ce domaine pour l'instance `instance_dir` : marqué par
/// ce Virtum, ou non marqué (créé avant le marqueur) et utilisant le disque de cette
/// instance. Un domaine d'un autre Virtum, ou non marqué et étranger, est conservé.
pub fn domain_is_ours_for_instance(vm_id: &str, instance_dir: &Path) -> bool {
    match domain_owner(vm_id) {
        Owner::Us => true,
        Owner::Other(_) => false,
        Owner::Unknown => crate::libvirt::virsh_command()
            .args(["dumpxml", "--inactive", vm_id])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .is_some_and(|o| {
                xml_uses_disk(
                    &String::from_utf8_lossy(&o.stdout),
                    &instance_dir.join("disk.qcow2"),
                )
            }),
    }
}

/// Refuse d'arrêter, supprimer ou remplacer le domaine `vm_id` de l'instance `instance_dir`
/// s'il existe et n'appartient pas à cet état Virtum (marqueur d'un autre Virtum, ou sans
/// marqueur et sans le disque de cette instance). L'entrée `metadata.json` est conservée.
pub fn ensure_domain_ours(vm_id: &str, instance_dir: &Path) -> anyhow::Result<()> {
    if !crate::libvirt::domain_exists(vm_id) || domain_is_ours_for_instance(vm_id, instance_dir) {
        return Ok(());
    }
    let owner = match domain_owner(vm_id) {
        Owner::Other(id) => tr!(fr: " (propriétaire : {id})", en: " (owner: {id})"),
        _ => tr!(
            fr: " (sans marqueur Virtum, disque d'une autre instance)",
            en: " (no Virtum marker, disk of another instance)"
        ),
    };
    anyhow::bail!(tr!(
        fr: "Le domaine libvirt {vm_id} n'appartient pas à cet état Virtum{owner} : refus d'y toucher. \
         Le nom est pris par une autre VM du même libvirt ; renommez le lab (`name:`) ou retirez \
         l'instance de cet état.",
        en: "The libvirt domain {vm_id} does not belong to this Virtum state{owner}: refusing to touch it. \
         The name is taken by another VM on the same libvirt; rename the lab (`name:`) or remove \
         the instance from this state."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_disk_match_is_exact() {
        let dir = Path::new("/var/lib/virtum/instances/vm-0123456789ab");
        let xml = "<disk type='file'><source file='/var/lib/virtum/instances/vm-0123456789ab/disk.qcow2'/></disk>";
        assert!(xml_uses_disk(xml, dir.join("disk.qcow2").as_path()));
        let other = "<source file='/var/lib/virtum/instances/vm-0123456789abc/disk.qcow2'/>";
        assert!(!xml_uses_disk(other, dir.join("disk.qcow2").as_path()));
    }

    #[test]
    fn parses_owner_id_from_libvirt_output() {
        let single = format!("<virtum:owner xmlns:virtum='{OWNER_NS}' id='1000:/var/lib/virtum'/>");
        assert_eq!(
            parse_owner_id(&single).as_deref(),
            Some("1000:/var/lib/virtum")
        );
        let double = format!(
            "<virtum:owner xmlns:virtum=\"{OWNER_NS}\" id=\"1001:/home/a&amp;b/.virtum\"/>"
        );
        assert_eq!(
            parse_owner_id(&double).as_deref(),
            Some("1001:/home/a&b/.virtum")
        );
        assert_eq!(parse_owner_id("<other/>"), None);
        // Sortie réelle de `virsh metadata <dom> <ns>` (libvirt 10).
        assert_eq!(
            parse_owner_id("<owner id=\"1000:/var/lib/virtum\"/>\n").as_deref(),
            Some("1000:/var/lib/virtum")
        );
    }

    #[test]
    fn metadata_block_round_trips() {
        let xml = metadata_xml("  ");
        assert!(xml.contains(OWNER_NS));
        assert_eq!(parse_owner_id(&xml), Some(owner_tag()));
    }
}
