use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs;

use crate::metadata::{self, Metadata};

/// Emplacement recommandé pour libvirt système (hors $HOME, lisible par libvirt-qemu).
pub const VAR_LIB_VIRTUM: &str = "/var/lib/virtum";

/// Resolved paths under the Virtum state root.
#[derive(Debug, Clone)]
pub struct VirtumPaths {
    pub root: PathBuf,
    pub images: PathBuf,
    pub instances: PathBuf,
    pub keys: PathBuf,
    pub metadata: PathBuf,
}

impl VirtumPaths {
    pub fn resolve() -> Result<Self> {
        let root = virtum_home()?;
        Ok(Self {
            images: root.join("images"),
            instances: root.join("instances"),
            keys: root.join("keys"),
            metadata: root.join("metadata.json"),
            root,
        })
    }
}

/// Ordre : `$VIRTUM_STATE_DIR` → `/var/lib/virtum` (si présent) → `~/.virtum`.
pub fn virtum_home() -> Result<PathBuf> {
    if let Ok(dir) = env::var("VIRTUM_STATE_DIR") {
        if dir.is_empty() {
            anyhow::bail!("VIRTUM_STATE_DIR est vide.");
        }
        return Ok(PathBuf::from(dir));
    }

    let var_lib = PathBuf::from(VAR_LIB_VIRTUM);
    if var_lib.is_dir() {
        return Ok(var_lib);
    }

    let home = dirs::home_dir()
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .context(
            "Impossible de déterminer le répertoire personnel ($HOME). \
             Définissez HOME, créez /var/lib/virtum (scripts/setup-var-lib.sh) \
             ou VIRTUM_STATE_DIR.",
        )?;
    Ok(home.join(".virtum"))
}

/// True si le stockage est sous `/var/lib/virtum` (compatible QEMU système).
pub fn uses_var_lib_storage() -> bool {
    virtum_home()
        .map(|p| p.starts_with(VAR_LIB_VIRTUM))
        .unwrap_or(false)
}

/// Creates `{root}/{images,instances,keys}` and initializes `metadata.json` if absent.
pub async fn ensure_layout() -> Result<VirtumPaths> {
    let paths = VirtumPaths::resolve()?;

    fs::create_dir_all(&paths.root)
        .await
        .with_context(|| format!("Impossible de créer {}", paths.root.display()))?;
    fs::create_dir_all(&paths.images)
        .await
        .with_context(|| format!("Impossible de créer {}", paths.images.display()))?;
    fs::create_dir_all(&paths.instances)
        .await
        .with_context(|| format!("Impossible de créer {}", paths.instances.display()))?;
    fs::create_dir_all(&paths.keys)
        .await
        .with_context(|| format!("Impossible de créer {}", paths.keys.display()))?;

    init_metadata_if_missing(&paths.metadata).await?;

    Ok(paths)
}

/// Synchronous entry point for non-async callers (`doctor`, CLI dispatch).
pub fn ensure_layout_blocking() -> Result<VirtumPaths> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .context("Impossible de démarrer le runtime Tokio")?
        .block_on(ensure_layout())
}

async fn init_metadata_if_missing(metadata_path: &Path) -> Result<()> {
    if !fs::try_exists(metadata_path).await? {
        metadata::save_metadata(metadata_path, &Metadata::default()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::isolated_virtum_state;

    #[tokio::test]
    async fn ensure_layout_is_idempotent() {
        let state = isolated_virtum_state();

        let paths1 = ensure_layout().await.unwrap();
        let paths2 = ensure_layout().await.unwrap();
        assert_eq!(paths1.root, paths2.root);
        assert_eq!(paths1.root, state.root().to_path_buf());
        assert!(paths1.metadata.is_file());
        let metadata = metadata::load_metadata(&paths1.metadata).await.unwrap();
        assert!(metadata.instances.is_empty());
    }
}
