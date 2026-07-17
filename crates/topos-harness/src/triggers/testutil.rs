//! Shared test doubles for the trigger modules — the same style the big-three adapter tests
//! use: a path-keyed in-memory [`ConfigStore`] (the pure-merge tests), a failing store (the
//! unreadable-config degrade probe), a real-disk store (the paths where a `std::fs` unlink must
//! be observable), and an RAII temp home. The crash-safe write itself is exercised by the CLI's
//! fault-injection sweep, where the real syscalls live.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::ConfigStore;

/// A path-keyed in-memory [`ConfigStore`] with a write counter (zero-write assertions).
#[derive(Debug, Default)]
pub(crate) struct MemConfig {
    files: RefCell<HashMap<PathBuf, Vec<u8>>>,
    writes: RefCell<u32>,
}

impl MemConfig {
    pub(crate) fn with_file(path: &str, bytes: &str) -> Self {
        let me = Self::default();
        me.set(path, bytes);
        me
    }
    pub(crate) fn set(&self, path: &str, bytes: &str) {
        self.set_raw(path, bytes.as_bytes());
    }
    /// Raw bytes, for the non-UTF-8 degrade probes.
    pub(crate) fn set_raw(&self, path: &str, bytes: &[u8]) {
        self.files
            .borrow_mut()
            .insert(PathBuf::from(path), bytes.to_vec());
    }
    pub(crate) fn text(&self, path: &str) -> Option<String> {
        self.files
            .borrow()
            .get(Path::new(path))
            .map(|b| String::from_utf8(b.clone()).unwrap())
    }
    pub(crate) fn writes(&self) -> u32 {
        *self.writes.borrow()
    }
}

impl ConfigStore for MemConfig {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        Ok(self.files.borrow().get(path).cloned())
    }
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.files
            .borrow_mut()
            .insert(path.to_path_buf(), bytes.to_vec());
        *self.writes.borrow_mut() += 1;
        Ok(())
    }
}

/// A store whose every access FAILS (a permission error, say) — the genuine-I/O-failure degrade
/// probe (distinct from absent).
#[derive(Debug)]
pub(crate) struct ErrConfig;

impl ConfigStore for ErrConfig {
    fn read(&self, _: &Path) -> io::Result<Option<Vec<u8>>> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
    }
    fn replace(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
    }
}

/// A real-disk [`ConfigStore`] over a temp home, for the tests where the file-drop `std::fs`
/// unlink must be observable.
#[derive(Debug)]
pub(crate) struct DiskConfig;

impl ConfigStore for DiskConfig {
    fn read(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)
    }
}

/// A self-cleaning temp dir (RAII).
pub(crate) struct TempHome(pub(crate) PathBuf);

impl TempHome {
    pub(crate) fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-trig-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
