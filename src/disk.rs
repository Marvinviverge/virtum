//! Copy-on-Write instance disks via qemu-img (step 10).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::command::{path_arg, run_command};

const MIN_DISK_BYTES: u64 = 1 << 30; // 1 GiB
const MAX_DISK_BYTES: u64 = 500 << 30; // 500 GiB

/// Taille de disque parsée pour `qemu-img create` et validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskSize {
    pub bytes: u64,
    /// Argument taille pour qemu-img (ex. `10G`).
    pub qemu_arg: String,
}

/// Parse `--disk-size` : `10G`, `512M`, `20480M`, etc.
pub fn parse_disk_size(input: &str) -> Result<DiskSize> {
    let s = input.trim();
    if s.is_empty() {
        bail!("--disk-size ne peut pas être vide.");
    }
    let (num_part, unit_part) = split_size_token(s)?;
    let number: u64 = num_part
        .parse()
        .with_context(|| format!("Taille de disque invalide: '{input}'"))?;
    if number == 0 {
        bail!("--disk-size doit être supérieur à zéro.");
    }
    let multiplier = match unit_part.to_ascii_uppercase().as_str() {
        "" | "B" => 1u64,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1 << 40,
        other => bail!("Unité de disque inconnue dans '{input}' : '{other}'"),
    };
    let bytes = number
        .checked_mul(multiplier)
        .with_context(|| format!("--disk-size trop grand ou invalide: '{input}'"))?;
    if bytes < MIN_DISK_BYTES {
        bail!(
            "--disk-size minimal: {} (ex. --disk-size 10G)",
            format_size_gib(MIN_DISK_BYTES)
        );
    }
    if bytes > MAX_DISK_BYTES {
        bail!("--disk-size maximal: 500G");
    }
    let qemu_arg = format_qemu_size_arg(number, unit_part);
    Ok(DiskSize { bytes, qemu_arg })
}

fn split_size_token(s: &str) -> Result<(&str, &str)> {
    let end_num = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end_num == 0 {
        bail!("--disk-size invalide: '{s}'");
    }
    Ok((&s[..end_num], &s[end_num..]))
}

fn format_qemu_size_arg(number: u64, unit: &str) -> String {
    if unit.is_empty() {
        format!("{number}")
    } else {
        let u = unit.to_ascii_uppercase();
        let suffix = match u.as_str() {
            "KB" | "KIB" => "K",
            "MB" | "MIB" => "M",
            "GB" | "GIB" => "G",
            "TB" | "TIB" => "T",
            other => other,
        };
        format!("{number}{suffix}")
    }
}

fn format_size_gib(bytes: u64) -> String {
    format_virtual_size_label(bytes)
}

/// Libellé lisible pour `virtum ps` (ex. `10G`, `3G`, `512M`).
pub fn format_virtual_size_label(bytes: u64) -> String {
    if bytes >= 1 << 30 && bytes.is_multiple_of(1 << 30) {
        format!("{}G", bytes >> 30)
    } else if bytes >= 1 << 20 && bytes.is_multiple_of(1 << 20) {
        format!("{}M", bytes >> 20)
    } else if bytes >= 1 << 10 && bytes.is_multiple_of(1 << 10) {
        format!("{}K", bytes >> 10)
    } else {
        format!("{bytes}B")
    }
}

/// Taille virtuelle de l'image de base (qemu-img info --output=json).
pub fn image_virtual_size_bytes(path: &Path) -> Result<u64> {
    let output = run_command("qemu-img", &["info", "--output=json", path_arg(path)?])?;
    if !output.status.success() {
        bail!(
            "qemu-img info a échoué pour {} : {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let json: Value = serde_json::from_slice(&output.stdout)
        .context("Impossible de parser la sortie qemu-img info (JSON)")?;
    json.get("virtual-size")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Champ virtual-size absent dans qemu-img info pour {}",
                path.display()
            )
        })
}

/// Creates `disk.qcow2` as a COW overlay on the read-only base cloud image.
///
/// Si `size` est `Some`, la taille virtuelle du disque instance est fixée (doit être
/// >= à l'image de base). cloud-init doit agrandir la partition si `size` > base.
pub fn create_cow_disk(base_image: &Path, disk_path: &Path, size: Option<&DiskSize>) -> Result<()> {
    let parent = disk_path
        .parent()
        .with_context(|| format!("Répertoire parent absent pour {}", disk_path.display()))?;
    if !parent.is_dir() {
        bail!(
            "Le répertoire d'instance n'existe pas: {}. \
             Créez-le avant d'appeler create_cow_disk.",
            parent.display()
        );
    }

    if disk_path.exists() {
        bail!("Le disque instance existe déjà: {}", disk_path.display());
    }

    let mut args = vec![
        "create",
        "-f",
        "qcow2",
        "-F",
        "qcow2",
        "-b",
        path_arg(base_image)?,
        path_arg(disk_path)?,
    ];

    if let Some(sz) = size {
        args.push(sz.qemu_arg.as_str());
    }

    run_command("qemu-img", &args).with_context(|| {
        format!(
            "Impossible de créer le disque COW {} depuis {}",
            disk_path.display(),
            base_image.display()
        )
    })?;

    Ok(())
}

/// Agrandit un disque qcow2 existant. Retourne `true` si la partition invitée doit être étendue (growpart).
pub fn resize_cow_disk(disk_path: &Path, target: &DiskSize) -> Result<bool> {
    if !disk_path.is_file() {
        bail!("Disque instance introuvable: {}", disk_path.display());
    }
    let current = image_virtual_size_bytes(disk_path)?;
    if target.bytes < current {
        bail!(
            "Réduction du disque non supportée (actuel {}, demandé {}). \
             Utilisez `virtum down --destroy` puis `virtum up` pour recréer la machine.",
            format_virtual_size_label(current),
            target.qemu_arg
        );
    }
    if target.bytes == current {
        return Ok(false);
    }
    run_command(
        "qemu-img",
        &["resize", path_arg(disk_path)?, target.qemu_arg.as_str()],
    )
    .with_context(|| {
        format!(
            "qemu-img resize a échoué pour {} → {}",
            disk_path.display(),
            target.qemu_arg
        )
    })?;
    Ok(true)
}

/// Valide `--disk-size` par rapport à l'image de base ; indique si growpart est nécessaire.
pub fn plan_disk_size(base_image: &Path, size: &DiskSize) -> Result<bool> {
    let base_bytes = image_virtual_size_bytes(base_image)?;
    if size.bytes < base_bytes {
        bail!(
            "--disk-size {} est inférieur à la taille de l'image de base ({}). \
             Utilisez au moins {}.",
            size.qemu_arg,
            format_size_gib(base_bytes),
            format_size_gib(base_bytes)
        );
    }
    Ok(size.bytes > base_bytes)
}

/// Returns true if `qemu-img info` reports the expected backing file.
pub fn cow_disk_has_backing_file(disk_path: &Path, base_image: &Path) -> Result<bool> {
    let output = run_command("qemu-img", &["info", path_arg(disk_path)?])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let base = base_image.to_string_lossy();
    Ok(stdout.contains(&format!("backing file: {base}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_disk_size_accepts_common_units() {
        assert_eq!(parse_disk_size("10G").unwrap().bytes, 10 << 30);
        assert_eq!(parse_disk_size("2G").unwrap().bytes, 2 << 30);
        assert_eq!(parse_disk_size("1G").unwrap().qemu_arg, "1G");
    }

    #[test]
    fn parse_disk_size_rejects_too_small() {
        assert!(parse_disk_size("512M").is_err());
    }

    #[test]
    fn create_cow_disk_requires_parent_directory() {
        let dir = std::env::temp_dir().join(format!("virtum-disk-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let base = dir.join("base.qcow2");
        let disk = dir.join("missing-parent").join("disk.qcow2");
        let err = create_cow_disk(&base, &disk, None).unwrap_err();
        assert!(err.to_string().contains("n'existe pas"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_cow_disk_with_qemu_img() {
        let dir = std::env::temp_dir().join(format!("virtum-disk-cow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.qcow2");
        let disk = dir.join("disk.qcow2");
        run_command(
            "qemu-img",
            &["create", "-f", "qcow2", path_arg(&base).unwrap(), "1G"],
        )
        .unwrap();
        create_cow_disk(&base, &disk, None).unwrap();
        assert!(disk.is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_cow_disk_with_explicit_size() {
        let dir = std::env::temp_dir().join(format!("virtum-disk-sized-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.qcow2");
        let disk = dir.join("disk.qcow2");
        run_command(
            "qemu-img",
            &["create", "-f", "qcow2", path_arg(&base).unwrap(), "1G"],
        )
        .unwrap();
        let size = parse_disk_size("10G").unwrap();
        create_cow_disk(&base, &disk, Some(&size)).unwrap();
        assert_eq!(image_virtual_size_bytes(&disk).unwrap(), size.bytes);
        let _ = fs::remove_dir_all(&dir);
    }
}
