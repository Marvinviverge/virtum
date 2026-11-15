//! Cloud image resolution, download (`virtum pull`) and cache (steps 8–9, 26).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256, Sha512};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::command::run_command;
use crate::libvirt_access;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestAlgo {
    Sha512,
    Sha256,
}

#[derive(Debug, Clone, Copy)]
struct OsImageSpec {
    id: &'static str,
    filename: &'static str,
    url: &'static str,
    digest: &'static str,
    algo: DigestAlgo,
    source_label: &'static str,
}

const OS_REGISTRY: &[OsImageSpec] = &[
    OsImageSpec {
        id: "debian-12",
        filename: "debian-12.qcow2",
        url: "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-amd64.qcow2",
        digest: "c39495a4d81927810e93f825b0b135a08e798f46dc748e78d6bfa2984f93ffc4bcfdc7a6b7f97666ad66fbae3e0f21debd24466ce1d844d0de7856fdc7537d58",
        algo: DigestAlgo::Sha512,
        source_label: "Debian Cloud",
    },
    OsImageSpec {
        id: "ubuntu-24.04",
        filename: "ubuntu-24.04.qcow2",
        url: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img",
        digest: "53fdde898feed8b027d94baa9cfe8229867f330a1d9c49dc7d84465ee7f229f7",
        algo: DigestAlgo::Sha256,
        source_label: "Ubuntu Cloud Images",
    },
    // Version datée (et non `release/`, qui change à chaque nouvelle image) : l'empreinte reste valable.
    // SHA256SUMS vérifié avec la clé de signature des images Ubuntu (ubuntu-cloudimage-keyring).
    OsImageSpec {
        id: "ubuntu-26.04",
        filename: "ubuntu-26.04.qcow2",
        url: "https://cloud-images.ubuntu.com/releases/26.04/release-20261108/ubuntu-26.04-server-cloudimg-amd64.img",
        digest: "7b90593f5532b49b7e3fe3d2743f0d1f0d822e5b07cebcd8e00c0df8069ecab9",
        algo: DigestAlgo::Sha256,
        source_label: "Ubuntu Cloud Images",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    pub os: String,
    pub local_path: PathBuf,
    pub url: &'static str,
    pub digest: &'static str,
    algo: DigestAlgo,
    source_label: &'static str,
}

/// Liste des identifiants `os` supportés (Virtumfile, `run`, `pull`).
pub fn supported_os_ids() -> Vec<&'static str> {
    OS_REGISTRY.iter().map(|s| s.id).collect()
}

fn format_supported_os_list() -> String {
    supported_os_ids().join(", ")
}

/// Maps `--os` / `virtum pull <os>` to local path, download URL and pinned digest.
pub fn resolve_image(os: &str, images_dir: &Path) -> Result<ResolvedImage> {
    let os = os.trim();
    for spec in OS_REGISTRY {
        if spec.id == os {
            return Ok(ResolvedImage {
                os: os.to_string(),
                local_path: images_dir.join(spec.filename),
                url: spec.url,
                digest: spec.digest,
                algo: spec.algo,
                source_label: spec.source_label,
            });
        }
    }
    bail!(
        "OS non supporté: '{os}'. OS disponibles : {}.",
        format_supported_os_list()
    );
}

/// Télécharge et met en cache l'image sans lancer de VM (`virtum pull`).
pub async fn pull_image(os: &str, images_dir: &Path) -> Result<PathBuf> {
    let image = resolve_image(os, images_dir)?;
    if fs::try_exists(&image.local_path).await? {
        set_libvirt_image_access(&image.local_path).await?;
        validate_base_image(&image).await?;
        crate::verr!(
            "Image {} déjà en cache: {}",
            image.os,
            image.local_path.display()
        );
        return Ok(image.local_path);
    }
    ensure_image_download(&image).await
}

/// Ensures the cloud image is cached under `~/.virtum/images/`.
pub async fn ensure_image(image: &ResolvedImage) -> Result<PathBuf> {
    if fs::try_exists(&image.local_path).await? {
        set_libvirt_image_access(&image.local_path).await?;
        validate_base_image(image).await?;
        return Ok(image.local_path.clone());
    }
    ensure_image_download(image).await
}

async fn ensure_image_download(image: &ResolvedImage) -> Result<PathBuf> {
    let parent = image
        .local_path
        .parent()
        .context("Chemin image sans répertoire parent")?;
    fs::create_dir_all(parent).await?;

    let tmp_path = image.local_path.with_extension("qcow2.tmp");
    if fs::try_exists(&tmp_path).await? {
        fs::remove_file(&tmp_path).await.ok();
    }

    crate::verr!(
        "Téléchargement de l'image {} depuis {}…",
        image.os,
        image.source_label
    );
    crate::verr!("URL: {}", image.url);

    download_and_verify(image, &tmp_path).await?;
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
    record_verified(image)?;

    crate::verr!(
        "Image {} installée: {}",
        image.os,
        image.local_path.display()
    );
    Ok(image.local_path.clone())
}

async fn download_and_verify(image: &ResolvedImage, dest: &Path) -> Result<()> {
    let response = reqwest::get(image.url)
        .await
        .with_context(|| format!("Échec du téléchargement depuis {}", image.url))?;
    if !response.status().is_success() {
        bail!(
            "Téléchargement refusé (HTTP {}): {}",
            response.status(),
            image.url
        );
    }

    // Nouveau fichier uniquement, sans suivre de lien planté à cet emplacement.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)
        .await
        .with_context(|| format!("Impossible de créer {}", dest.display()))?;
    let mut hasher: Box<dyn DynDigest> = match image.algo {
        DigestAlgo::Sha512 => Box::new(Sha512::new()),
        DigestAlgo::Sha256 => Box::new(Sha256::new()),
    };
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();

    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Erreur pendant le téléchargement de l'image")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        if downloaded > 0 && downloaded.is_multiple_of(50 * 1024 * 1024) {
            crate::verr!("  … {} MiB téléchargés", downloaded / (1024 * 1024));
        }
    }

    file.sync_all().await?;
    drop(file);

    let digest = hasher.finalize_hex();
    let algo_name = match image.algo {
        DigestAlgo::Sha512 => "SHA512",
        DigestAlgo::Sha256 => "SHA256",
    };
    if digest != image.digest {
        fs::remove_file(dest).await.ok();
        bail!(
            "Checksum {algo_name} invalide pour l'image téléchargée.\n\
             attendu: {}\n\
             obtenu:  {}\n\
             L'image n'a pas été installée.",
            image.digest,
            digest
        );
    }

    Ok(())
}

trait DynDigest {
    fn update(&mut self, data: &[u8]);
    fn finalize_hex(self: Box<Self>) -> String;
}

impl DynDigest for Sha512 {
    fn update(&mut self, data: &[u8]) {
        Digest::update(self, data);
    }
    fn finalize_hex(self: Box<Self>) -> String {
        hex::encode(self.finalize())
    }
}

impl DynDigest for Sha256 {
    fn update(&mut self, data: &[u8]) {
        Digest::update(self, data);
    }
    fn finalize_hex(self: Box<Self>) -> String {
        hex::encode(self.finalize())
    }
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

/// Image en cache réutilisée sans retéléchargement.
///
/// Le condensat épinglé n'est vérifié qu'au téléchargement ; ensuite l'image peut changer
/// de propriétaire (libvirt `dynamic_ownership` la passe souvent à `libvirt-qemu:kvm` en
/// 0664) et donc devenir modifiable par d'autres. Avant chaque réemploi :
/// - fichier régulier (pas un lien), format qcow2, **sans** fichier de base ;
/// - si inode, taille, mtime ou ctime ont changé depuis la dernière vérification (tampon
///   dans un fichier de l'utilisateur), le condensat épinglé est recalculé.
async fn validate_base_image(image: &ResolvedImage) -> Result<()> {
    let path = &image.local_path;
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("Impossible de lire {}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!(
            "L'image en cache {} n'est pas un fichier régulier. Supprimez-la puis relancez \
             `virtum pull`.",
            path.display()
        );
    }
    let output = run_command("qemu-img", &["info", "-U", crate::command::path_arg(path)?])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains("file format: qcow2") {
        bail!(
            "qemu-img info n'a pas confirmé le format qcow2 pour {}",
            path.display()
        );
    }
    if stdout
        .lines()
        .any(|l| l.trim_start().starts_with("backing file:"))
    {
        bail!(
            "L'image en cache {} déclare un fichier de base qcow2 : refusée. Supprimez-la puis \
             relancez `virtum pull`.",
            path.display()
        );
    }
    if stamp_matches(image, &meta) {
        return Ok(());
    }
    crate::verr!(
        "Vérification du condensat de l'image {} (modifiée depuis la dernière vérification)…",
        image.os
    );
    let (algo, expected) = (image.algo, image.digest);
    let p = path.clone();
    let digest = tokio::task::spawn_blocking(move || hash_file(&p, algo))
        .await
        .context("spawn_blocking hash image")??;
    if digest != expected {
        bail!(
            "L'image en cache {} ne correspond plus au condensat épinglé (modifiée ?).\n\
             attendu: {expected}\nobtenu:  {digest}\n\
             Supprimez-la puis relancez `virtum pull`.",
            path.display()
        );
    }
    record_verified(image)
}

/// Tampon de vérification : fichier de l'utilisateur à côté des images.
fn stamp_path(image: &ResolvedImage) -> PathBuf {
    image
        .local_path
        .with_file_name(format!(".{}.verified.json", image.os))
}

fn stamp_of(meta: &std::fs::Metadata, digest: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}.{}:{}.{}:{digest}",
        meta.dev(),
        meta.ino(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec()
    )
}

fn stamp_matches(image: &ResolvedImage, meta: &std::fs::Metadata) -> bool {
    crate::fsutil::read_owned(&stamp_path(image))
        .ok()
        .flatten()
        .is_some_and(|s| s.trim() == stamp_of(meta, image.digest))
}

fn record_verified(image: &ResolvedImage) -> Result<()> {
    let meta = std::fs::symlink_metadata(&image.local_path)
        .with_context(|| format!("Impossible de lire {}", image.local_path.display()))?;
    crate::fsutil::write_atomic(
        &stamp_path(image),
        format!("{}\n", stamp_of(&meta, image.digest)).as_bytes(),
        0o644,
    )
}

fn hash_file(path: &Path, algo: DigestAlgo) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Impossible d'ouvrir {}", path.display()))?;
    let mut hasher: Box<dyn DynDigest> = match algo {
        DigestAlgo::Sha512 => Box::new(Sha512::new()),
        DigestAlgo::Sha256 => Box::new(Sha256::new()),
    };
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize_hex())
}

/// Base image readable by the system libvirt QEMU user (group `libvirt`, mode 664).
async fn set_libvirt_image_access(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || libvirt_access::set_image_file_for_libvirt(&path))
        .await
        .context("spawn_blocking set_image_file_for_libvirt")??;
    Ok(())
}

pub fn pull(os: &str) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(pull_async(os))
}

async fn pull_async(os: &str) -> Result<()> {
    let paths = crate::config::ensure_layout().await?;
    pull_image(os, &paths.images).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_debian_12() {
        let dir = Path::new("/tmp/virtum-images");
        let image = resolve_image("debian-12", dir).unwrap();
        assert_eq!(image.local_path, dir.join("debian-12.qcow2"));
        assert_eq!(image.url, OS_REGISTRY[0].url);
        assert_eq!(image.algo, DigestAlgo::Sha512);
    }

    #[test]
    fn resolve_ubuntu_24_04() {
        let dir = Path::new("/tmp/virtum-images");
        let image = resolve_image("ubuntu-24.04", dir).unwrap();
        assert_eq!(image.local_path, dir.join("ubuntu-24.04.qcow2"));
        assert_eq!(image.algo, DigestAlgo::Sha256);
    }

    #[test]
    fn resolve_unknown_os_fails() {
        let err = resolve_image("arch-linux", Path::new("/tmp")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("non supporté"));
        assert!(msg.contains("debian-12"));
        assert!(msg.contains("ubuntu-24.04"));
    }

    #[test]
    fn supported_os_ids_lists_registry() {
        assert_eq!(
            supported_os_ids(),
            vec!["debian-12", "ubuntu-24.04", "ubuntu-26.04"]
        );
    }

    #[test]
    fn resolve_ubuntu_26_04() {
        let dir = Path::new("/tmp/virtum-images");
        let image = resolve_image("ubuntu-26.04", dir).unwrap();
        assert_eq!(image.local_path, dir.join("ubuntu-26.04.qcow2"));
        assert_eq!(image.algo, DigestAlgo::Sha256);
        // version datée : l'empreinte ne périme pas quand Canonical publie une nouvelle image
        assert!(image.url.contains("/release-20261108/"));
    }
}
