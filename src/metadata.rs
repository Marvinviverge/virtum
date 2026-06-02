//! Global instance state in `~/.virtum/metadata.json`.
//!
//! Public API is wired into VM lifecycle commands in later STEPS (6–19).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Creating,
    Running,
    Stopped,
    Destroyed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub id: String,
    pub os: String,
    pub status: InstanceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// Port TCP hôte pour SSH (redirection SLIRP) ; absent ou 22 = port SSH standard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    pub user: String,
    pub hostname: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub instances: Vec<Instance>,
}

/// Loads `metadata.json`. Returns an empty state if the file does not exist.
pub async fn load_metadata(path: &Path) -> Result<Metadata> {
    if !fs::try_exists(path).await? {
        return Ok(Metadata::default());
    }

    let content = fs::read_to_string(path)
        .await
        .with_context(|| format!("Impossible de lire {}", path.display()))?;

    serde_json::from_str(&content).with_context(|| {
        format!(
            "metadata.json invalide (JSON malformé ou schéma incorrect): {}",
            path.display()
        )
    })
}

/// Persists metadata atomically: `.tmp` → fsync → rename.
pub async fn save_metadata_atomic(path: &Path, metadata: &Metadata) -> Result<()> {
    let tmp_path = metadata_tmp_path(path);
    let content =
        serde_json::to_string_pretty(metadata).context("Impossible de sérialiser metadata.json")?;

    let mut file = fs::File::create(&tmp_path)
        .await
        .with_context(|| format!("Impossible de créer {}", tmp_path.display()))?;
    file.write_all(format!("{content}\n").as_bytes()).await?;
    file.sync_all().await?;
    drop(file);

    fs::rename(&tmp_path, path).await.with_context(|| {
        format!(
            "Impossible de renommer {} vers {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// All metadata writes go through the atomic path (step 6).
pub async fn save_metadata(path: &Path, metadata: &Metadata) -> Result<()> {
    save_metadata_atomic(path, metadata).await
}

fn metadata_tmp_path(path: &Path) -> PathBuf {
    path.parent()
        .map(|dir| dir.join("metadata.json.tmp"))
        .unwrap_or_else(|| PathBuf::from("metadata.json.tmp"))
}

pub fn find_instance<'a>(metadata: &'a Metadata, id: &str) -> Option<&'a Instance> {
    metadata.instances.iter().find(|instance| instance.id == id)
}

/// Inserts a new instance or replaces an existing one with the same `id`.
pub async fn upsert_instance(path: &Path, instance: Instance) -> Result<Metadata> {
    let mut metadata = load_metadata(path).await?;
    if let Some(existing) = metadata
        .instances
        .iter_mut()
        .find(|item| item.id == instance.id)
    {
        *existing = instance;
    } else {
        metadata.instances.push(instance);
    }
    save_metadata(path, &metadata).await?;
    Ok(metadata)
}

/// Removes an instance by `id`. Errors if the instance is not found.
pub async fn remove_instance(path: &Path, id: &str) -> Result<Metadata> {
    let mut metadata = load_metadata(path).await?;
    let before = metadata.instances.len();
    metadata.instances.retain(|instance| instance.id != id);
    if metadata.instances.len() == before {
        bail!("Instance '{id}' introuvable dans metadata.json");
    }
    save_metadata(path, &metadata).await?;
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::test_env::isolated_virtum_state;

    async fn temp_metadata_path() -> (std::path::PathBuf, crate::test_env::IsolatedVirtumState) {
        let state = isolated_virtum_state();
        let paths = config::ensure_layout().await.unwrap();
        assert_eq!(paths.root, state.root().to_path_buf());
        (paths.metadata, state)
    }

    fn sample_instance(id: &str) -> Instance {
        Instance {
            id: id.to_string(),
            os: "debian-12".to_string(),
            status: InstanceStatus::Running,
            ip: Some("192.168.122.45".to_string()),
            ssh_port: None,
            user: "virtum".to_string(),
            hostname: id.to_string(),
            created_at: "2026-06-02T10:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let (path, _state) = temp_metadata_path().await;
        let metadata = load_metadata(&path).await.unwrap();
        assert!(metadata.instances.is_empty());
    }

    #[tokio::test]
    async fn load_malformed_json_fails() {
        let (path, _state) = temp_metadata_path().await;
        std::fs::write(&path, "{ not json").unwrap();
        let err = load_metadata(&path).await.unwrap_err();
        assert!(err.to_string().contains("invalide"));
    }

    #[tokio::test]
    async fn find_upsert_and_remove_instance() {
        let (path, _state) = temp_metadata_path().await;

        let empty = load_metadata(&path).await.unwrap();
        assert!(find_instance(&empty, "vm-abc123").is_none());

        let instance = sample_instance("vm-abc123");
        upsert_instance(&path, instance.clone()).await.unwrap();
        let loaded = load_metadata(&path).await.unwrap();
        assert_eq!(find_instance(&loaded, "vm-abc123"), Some(&instance));

        let mut updated = instance.clone();
        updated.status = InstanceStatus::Stopped;
        updated.ip = None;
        upsert_instance(&path, updated.clone()).await.unwrap();
        let loaded = load_metadata(&path).await.unwrap();
        assert_eq!(find_instance(&loaded, "vm-abc123"), Some(&updated));

        remove_instance(&path, "vm-abc123").await.unwrap();
        let loaded = load_metadata(&path).await.unwrap();
        assert!(loaded.instances.is_empty());
    }

    #[tokio::test]
    async fn save_metadata_atomic_writes_valid_json() {
        let (path, _state) = temp_metadata_path().await;
        let instance = sample_instance("vm-atomic");
        upsert_instance(&path, instance).await.unwrap();

        let loaded = load_metadata(&path).await.unwrap();
        assert_eq!(loaded.instances.len(), 1);
        // tmp must not remain after successful save
        let tmp = metadata_tmp_path(&path);
        assert!(!tmp.exists());
    }

    #[tokio::test]
    async fn save_overwrites_stale_tmp_on_next_write() {
        let (path, _state) = temp_metadata_path().await;
        let tmp = metadata_tmp_path(&path);
        std::fs::write(&tmp, "stale tmp content").unwrap();

        let instance = sample_instance("vm-tmp");
        upsert_instance(&path, instance).await.unwrap();
        assert!(!tmp.exists());
        let loaded = load_metadata(&path).await.unwrap();
        assert_eq!(loaded.instances[0].id, "vm-tmp");
    }
}
