//! Cloud image resolution and download (steps 8–9).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha512};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::command::run_command;
use crate::libvirt_access;

const DEBIAN_12_OS: &str = "debian-12";
const DEBIAN_12_FILENAME: &str = "debian-12.qcow2";
const DEBIAN_12_URL: &str =
    "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-amd64.qcow2";
const DEBIAN_12_SHA512: &str = "c39495a4d81927810e93f825b0b135a08e798f46dc748e78d6bfa2984f93ffc4bcfdc7a6b7f97666ad66fbae3e0f21debd24466ce1d844d0de7856fdc7537d58";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    pub os: String,
    pub local_path: PathBuf,
    pub url: &'static str,
    pub expected_sha512: &'static str,
}

/// Maps `--os` to local path, download URL and pinned SHA512 (SPEC §2).
pub fn resolve_image(os: &str, images_dir: &Path) -> Result<ResolvedImage> {
    match os {
        DEBIAN_12_OS => Ok(ResolvedImage {
            os: os.to_string(),
            local_path: images_dir.join(DEBIAN_12_FILENAME),
            url: DEBIAN_12_URL,
            expected_sha512: DEBIAN_12_SHA512,
        }),
        _ => bail!("OS non supporté: '{os}'. Virtum v0.1 ne supporte que '{DEBIAN_12_OS}'."),
    }
}

/// Ensures the cloud image is cached under `~/.virtum/images/`.
pub async fn ensure_image(image: &ResolvedImage) -> Result<PathBuf> {
    if fs::try_exists(&image.local_path).await? {
        set_libvirt_image_access(&image.local_path).await?;
        validate_base_image(&image.local_path).await?;
        return Ok(image.local_path.clone());
    }

    let parent = image
        .local_path
        .parent()
        .context("Chemin image sans répertoire parent")?;
    fs::create_dir_all(parent).await?;

    let tmp_path = image.local_path.with_extension("qcow2.tmp");
    if fs::try_exists(&tmp_path).await? {
        fs::remove_file(&tmp_path).await.ok();
    }

    eprintln!(
        "Téléchargement de l'image {} depuis Debian Cloud…",
        image.os
    );
    eprintln!("URL: {}", image.url);

    download_and_verify(image.url, image.expected_sha512, &tmp_path).await?;
    validate_qcow2_with_qemu_img(&tmp_path).await?;

    fs::rename(&tmp_path, &image.local_path)
        .await
        .with_context(|| {
            format!(
                "Impossible de finaliser l'image vers {}",
                image.local_path.display()
            )
        })?;

    set_libvirt_image_access(&image.local_path).await?;
    validate_base_image(&image.local_path).await?;

    eprintln!(
        "Image {} installée: {}",
        image.os,
        image.local_path.display()
    );
    Ok(image.local_path.clone())
}

async fn download_and_verify(url: &str, expected_sha512: &str, dest: &Path) -> Result<()> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("Échec du téléchargement depuis {url}"))?;
    if !response.status().is_success() {
        bail!("Téléchargement refusé (HTTP {}): {url}", response.status());
    }

    let mut file = fs::File::create(dest)
        .await
        .with_context(|| format!("Impossible de créer {}", dest.display()))?;
    let mut hasher = Sha512::new();
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();

    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Erreur pendant le téléchargement de l'image")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        if downloaded > 0 && downloaded.is_multiple_of(50 * 1024 * 1024) {
            eprintln!("  … {} MiB téléchargés", downloaded / (1024 * 1024));
        }
    }

    file.sync_all().await?;
    drop(file);

    let digest = hex::encode(hasher.finalize());
    if digest != expected_sha512 {
        fs::remove_file(dest).await.ok();
        bail!(
            "Checksum SHA512 invalide pour l'image téléchargée.\n\
             attendu: {expected_sha512}\n\
             obtenu:  {digest}\n\
             L'image n'a pas été installée."
        );
    }

    Ok(())
}

async fn validate_qcow2_with_qemu_img(path: &Path) -> Result<()> {
    let output = run_command("qemu-img", &["info", crate::command::path_arg(path)?])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("file format: qcow2") {
        fs::remove_file(path).await.ok();
        bail!(
            "qemu-img info n'a pas confirmé le format qcow2 pour {}",
            path.display()
        );
    }
    Ok(())
}

async fn validate_base_image(path: &Path) -> Result<()> {
    validate_qcow2_with_qemu_img(path).await
}

/// Base image readable by the system libvirt QEMU user (group `libvirt`, mode 664).
async fn set_libvirt_image_access(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || libvirt_access::set_image_file_for_libvirt(&path))
        .await
        .context("spawn_blocking set_image_file_for_libvirt")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_debian_12() {
        let dir = Path::new("/tmp/virtum-images");
        let image = resolve_image("debian-12", dir).unwrap();
        assert_eq!(image.local_path, dir.join("debian-12.qcow2"));
        assert_eq!(image.url, DEBIAN_12_URL);
    }

    #[test]
    fn resolve_unknown_os_fails() {
        let err = resolve_image("ubuntu-24.04", Path::new("/tmp")).unwrap_err();
        assert!(err.to_string().contains("non supporté"));
    }
}
