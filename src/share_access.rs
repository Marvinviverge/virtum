//! Qui, en dehors de l'utilisateur Virtum, peut atteindre la racine d'un partage virtiofs.
//!
//! root dans l'invité peut déposer dans un partage un exécutable setuid/setgid appartenant
//! à l'utilisateur Virtum (virtiofsd tourne sous cet utilisateur). Pour qu'aucun autre compte
//! local ne puisse l'exécuter, la racine du partage ne doit être traversable par personne
//! d'autre : ni « autres », ni un groupe partagé, ni une entrée ACL nommée.

use std::ffi::CStr;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::tr;

/// `true` si le groupe `gid` contient un compte autre que `uid` : membre secondaire
/// (`/etc/group`) ou compte dont c'est le groupe principal (`/etc/passwd`).
pub fn group_has_other_members(gid: u32, uid: u32) -> bool {
    secondary_member_uids(gid)
        .into_iter()
        .chain(primary_member_uids(gid))
        .any(|member| member != uid)
}

fn secondary_member_uids(gid: u32) -> Vec<u32> {
    let mut uids = Vec::new();
    unsafe {
        let gr = libc::getgrgid(gid);
        if gr.is_null() {
            return uids;
        }
        let mut p = (*gr).gr_mem;
        while !p.is_null() && !(*p).is_null() {
            let name = CStr::from_ptr(*p).to_owned();
            let pw = libc::getpwnam(name.as_ptr());
            // Membre inconnu de passwd : on le compte comme « autre » (prudence).
            uids.push(if pw.is_null() { u32::MAX } else { (*pw).pw_uid });
            p = p.add(1);
        }
    }
    uids
}

fn primary_member_uids(gid: u32) -> Vec<u32> {
    let mut uids = Vec::new();
    unsafe {
        libc::setpwent();
        loop {
            let pw = libc::getpwent();
            if pw.is_null() {
                break;
            }
            if (*pw).pw_gid == gid {
                uids.push((*pw).pw_uid);
            }
        }
        libc::endpwent();
    }
    uids
}

/// Entrées ACL nommées (`user:<uid>:…`, `group:<gid>:…`) qui donnent la traversée (`x`)
/// de la racine du partage à quelqu'un d'autre que l'utilisateur courant.
///
/// `text` est la sortie de `getfacl -n -p --omit-header`. Le masque (`mask::`) borne les
/// droits effectifs des entrées nommées. Les entrées `default:` (héritées par les nouveaux
/// fichiers) sont ignorées : sans traversée de la racine, elles ne donnent aucun accès.
/// `group_is_ours(gid)` dit si un groupe nommé ne contient que l'utilisateur courant.
pub fn named_acl_traversers(
    text: &str,
    uid: u32,
    group_is_ours: impl Fn(u32) -> bool,
) -> Vec<String> {
    let mask_x = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("mask::"))
        .map(|perms| perms.contains('x'))
        .unwrap_or(true);
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with("default:") {
            continue;
        }
        let mut parts = line.splitn(3, ':');
        let (Some(kind), Some(qualifier), Some(perms)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if qualifier.is_empty() || !perms.contains('x') || !mask_x {
            continue;
        }
        let id = qualifier.parse::<u32>().ok();
        let allowed = match kind {
            "user" => id == Some(uid),
            "group" => id.is_some_and(&group_is_ours),
            _ => true,
        };
        if !allowed {
            found.push(line.to_string());
        }
    }
    found
}

/// Lit les ACL de `path` (`getfacl`). Sans `getfacl`, aucune ACL n'a pu être posée par les
/// outils usuels : liste vide.
pub fn named_acl_traversers_of(path: &Path, uid: u32) -> Result<Vec<String>> {
    let output = match Command::new("getfacl")
        .args(["-n", "-p", "--omit-header"])
        .arg(path)
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).context(tr!(
                fr: "Impossible d'exécuter getfacl",
                en: "Cannot run getfacl"
            ))
        }
    };
    if !output.status.success() {
        anyhow::bail!(tr!(
            fr: "getfacl a échoué sur {} : {}",
            en: "getfacl failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(named_acl_traversers(&text, uid, |gid| {
        !group_has_other_members(gid, uid)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_acl_entries_granting_traversal_are_reported() {
        let text = "user::rwx\nuser:1001:r-x\nuser:1000:rwx\ngroup::r-x\ngroup:2000:--x\n\
                    group:3000:r--\nmask::r-x\nother::---\ndefault:user:1002:rwx\n";
        let found = named_acl_traversers(text, 1000, |gid| gid == 4000);
        assert_eq!(found, vec!["user:1001:r-x", "group:2000:--x"]);
    }

    #[test]
    fn mask_without_x_neutralises_named_entries() {
        let text = "user::rwx\nuser:1001:r-x\ngroup::r--\nmask::r--\nother::---\n";
        assert!(named_acl_traversers(text, 1000, |_| false).is_empty());
    }

    #[test]
    fn plain_mode_has_no_named_entries() {
        let text = "user::rwx\ngroup::r-x\nother::---\n";
        assert!(named_acl_traversers(text, 1000, |_| false).is_empty());
    }

    #[test]
    fn named_group_with_only_us_is_allowed() {
        let text = "user::rwx\ngroup:4000:r-x\nmask::r-x\nother::---\n";
        assert!(named_acl_traversers(text, 1000, |gid| gid == 4000).is_empty());
    }
}
