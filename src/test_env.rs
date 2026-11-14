//! Test-only helpers: isolate state from `/var/lib/virtum` and `~/.virtum`.
//! Not used by the CLI; safe to call only from unit/integration tests.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn lock_home() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Temp Virtum root via `VIRTUM_STATE_DIR` (never uses `/var/lib/virtum` or `~/.virtum`).
pub struct IsolatedVirtumState {
    root: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl IsolatedVirtumState {
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Dossier temporaire créé exclusivement (mode 0700, nom imprévisible) : jamais un dossier
/// qu'un autre compte aurait créé d'avance dans un /tmp partagé.
pub fn private_temp_dir(tag: &str) -> std::path::PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    loop {
        let name = format!(
            "virtum-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let path = env::temp_dir().join(name);
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return path,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("dossier temporaire de test impossible : {e}"),
        }
    }
}

pub fn isolated_virtum_state() -> IsolatedVirtumState {
    let guard = lock_home();
    let root = private_temp_dir("state-test");
    // SAFETY: ENV_LOCK held; no concurrent test reads VIRTUM_STATE_DIR.
    unsafe { env::set_var("VIRTUM_STATE_DIR", &root) };
    IsolatedVirtumState {
        root,
        _guard: guard,
    }
}

impl Drop for IsolatedVirtumState {
    fn drop(&mut self) {
        unsafe { env::remove_var("VIRTUM_STATE_DIR") };
        let _ = fs::remove_dir_all(&self.root);
    }
}
