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

pub fn isolated_virtum_state() -> IsolatedVirtumState {
    let guard = lock_home();
    let root = env::temp_dir().join(format!("virtum-state-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
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
