//! Lecture / écriture des fichiers d'état Virtum sans suivre de lien symbolique.
//!
//! Les répertoires d'état peuvent être partagés (groupe `libvirt`, `kvm`) : un fichier
//! d'état n'est relu que s'il appartient à l'utilisateur courant, et n'est jamais écrit
//! à travers un lien symbolique planté à sa place.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Uid du processus courant.
pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Écrit `content` de façon atomique : fichier temporaire unique (`O_EXCL | O_NOFOLLOW`)
/// dans le même répertoire, puis `rename`. Un lien symbolique existant à `path` est
/// remplacé, jamais suivi.
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let tmp = unique_sibling(path)?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .with_context(|| format!("Impossible de créer {}", tmp.display()))?;
        file.write_all(content)
            .with_context(|| format!("Impossible d'écrire {}", tmp.display()))?;
        file.sync_all().ok();
        fs::rename(&tmp, path)
            .with_context(|| format!("Impossible de remplacer {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Crée un nouveau fichier temporaire (`O_EXCL | O_NOFOLLOW`, mode 0600) dans `dir`.
pub fn write_new_temp(dir: &Path, prefix: &str, content: &[u8]) -> Result<PathBuf> {
    let path = unique_in(dir, prefix)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("Impossible de créer {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("Impossible d'écrire {}", path.display()))?;
    Ok(path)
}

/// Lit un fichier d'état : `None` s'il est absent ; erreur si c'est un lien symbolique,
/// autre chose qu'un fichier, ou s'il n'appartient pas à l'utilisateur courant.
pub fn read_owned(path: &Path) -> Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Impossible de lire {}", path.display())),
    };
    ensure_owned_regular_file(path, &meta)?;
    let content = fs::read_to_string(path)
        .with_context(|| format!("Impossible de lire {}", path.display()))?;
    Ok(Some(content))
}

/// Refuse un lien symbolique, un non-fichier ou un fichier d'un autre utilisateur.
pub fn ensure_owned_regular_file(path: &Path, meta: &fs::Metadata) -> Result<()> {
    if meta.file_type().is_symlink() {
        bail!(
            "{} est un lien symbolique : fichier d'état refusé (possible altération).",
            path.display()
        );
    }
    if !meta.is_file() {
        bail!("{} n'est pas un fichier régulier.", path.display());
    }
    if meta.uid() != current_uid() {
        bail!(
            "{} appartient à l'uid {} et non à l'utilisateur courant : fichier d'état refusé \
             (possible altération). Supprimez-le s'il est légitime.",
            path.display(),
            meta.uid()
        );
    }
    Ok(())
}

fn unique_sibling(path: &Path) -> Result<PathBuf> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("Nom de fichier invalide")?;
    unique_in(dir, &format!(".{name}"))
}

fn unique_in(dir: &Path, prefix: &str) -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let rand = uuid::Uuid::new_v4().simple().to_string();
    Ok(dir.join(format!(
        "{prefix}.tmp-{}-{nanos}-{}",
        std::process::id(),
        &rand[..8]
    )))
}

/// Horodatage UTC des fichiers d'état, à la seconde (`2026-11-17T10:00:00Z`).
pub fn now_rfc3339() -> String {
    jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("virtum-fsutil-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_atomic_replaces_symlink_without_following_it() {
        let d = tmpdir("nofollow");
        let victim = d.join("victim");
        fs::write(&victim, "precious").unwrap();
        let state = d.join("config.xml");
        symlink(&victim, &state).unwrap();

        write_atomic(&state, b"<domain/>", 0o644).unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert!(!fs::symlink_metadata(&state)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&state).unwrap(), "<domain/>");
        let mode = fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn read_owned_rejects_symlink_and_accepts_own_file() {
        let d = tmpdir("readowned");
        let real = d.join("real.json");
        fs::write(&real, "[]").unwrap();
        assert_eq!(read_owned(&real).unwrap().as_deref(), Some("[]"));
        let link = d.join("link.json");
        symlink(&real, &link).unwrap();
        assert!(read_owned(&link).is_err());
        assert!(read_owned(&d.join("absent.json")).unwrap().is_none());
        let _ = fs::remove_dir_all(&d);
    }
}
