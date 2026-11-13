//! Marqueur de propriétaire Virtum dans les objets libvirt (domaines, réseaux de lab).
//!
//! Sur `qemu:///system`, l'espace de noms libvirt est partagé entre utilisateurs alors que
//! l'état Virtum (metadata.json) est propre à chaque racine d'état. Les noms des VM de lab
//! (`vm-<sha256(lab:machine)>`) et des réseaux (`virtum-lab-<lab>`) sont déterministes : deux
//! utilisateurs avec le même lab produisent les mêmes noms. Virtum n'arrête, ne supprime ni
//! ne recrée un objet libvirt qu'après avoir vérifié qu'il porte son marqueur.

use std::process::Command;

use crate::fsutil::current_uid;
use crate::libvirt::xml_escape;

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
    let output = Command::new("virsh")
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

/// Au moins un domaine libvirt (actif ou non) a une interface sur ce réseau.
pub fn network_has_attached_domains(net_name: &str) -> bool {
    let Ok(names) = crate::libvirt::list_domain_names() else {
        // Dans le doute, considérer le réseau comme utilisé (pas de recréation).
        return true;
    };
    names.iter().any(|dom| {
        Command::new("virsh")
            .args(["domiflist", dom])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .any(|tok| tok == net_name)
            })
            .unwrap_or(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
