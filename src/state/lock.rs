use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;

pub struct FileLock {
    file: File,
}

impl FileLock {
    pub fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .context("Failed to open lock file")?;
        Ok(Self { file })
    }

    pub fn lock_exclusive(&self) -> Result<()> {
        fs2::FileExt::lock_exclusive(&self.file)
            .context("Failed to acquire exclusive lock")?;
        Ok(())
    }

    pub fn lock_shared(&self) -> Result<()> {
        fs2::FileExt::lock_shared(&self.file)
            .context("Failed to acquire shared lock")?;
        Ok(())
    }

    pub fn unlock(&self) -> Result<()> {
        fs2::FileExt::unlock(&self.file)
            .context("Failed to release lock")?;
        Ok(())
    }
}
