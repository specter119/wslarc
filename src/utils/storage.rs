use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::utils::cli::MountInfo;

pub fn verify_mount(
    mount: Option<&MountInfo>,
    target: &str,
    fstype: &str,
    uuid: &str,
) -> Result<()> {
    let mount = mount.with_context(|| format!("{target} is not mounted"))?;
    if mount.target != target || mount.fstype != fstype || mount.uuid.as_deref() != Some(uuid) {
        bail!("Refusing unexpected filesystem at {target}: expected {fstype} UUID={uuid}");
    }
    Ok(())
}

pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().context("Destination has no parent")?;
    fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(content)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Keep the source inode open across replacements, even when a target aliases it.
pub fn install_copies(source: &Path, targets: &[PathBuf]) -> Result<()> {
    let mut source_file = File::open(source)?;
    let permissions = source_file.metadata()?.permissions();
    for target in targets {
        let parent = target
            .parent()
            .context("Binary destination has no parent")?;
        fs::create_dir_all(parent)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        source_file.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut source_file, staged.as_file_mut())?;
        staged.as_file().set_permissions(permissions.clone())?;
        staged.as_file().sync_all()?;
        staged.persist(target).map_err(|error| error.error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_install_preserves_aliased_source_for_all_copies() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wslarc");
        let second = dir.path().join("ext4/wslarc");
        fs::write(&source, b"new binary").unwrap();
        install_copies(&source, &[source.clone(), second.clone()]).unwrap();
        assert_eq!(fs::read(source).unwrap(), b"new binary");
        assert_eq!(fs::read(second).unwrap(), b"new binary");
    }

    #[test]
    fn binary_copy_failure_keeps_old_destination() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wslarc");
        fs::write(&target, b"old binary").unwrap();
        assert!(install_copies(dir.path(), std::slice::from_ref(&target)).is_err());
        assert_eq!(fs::read(target).unwrap(), b"old binary");
    }

    #[test]
    fn binary_install_survives_two_paths_to_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let source = alias.join("wslarc");
        let copy = dir.path().join("ext4/wslarc");
        fs::write(&source, b"shared inode").unwrap();
        install_copies(&source, &[real.join("wslarc"), copy.clone()]).unwrap();
        assert_eq!(fs::read(source).unwrap(), b"shared inode");
        assert_eq!(fs::read(copy).unwrap(), b"shared inode");
    }

    #[test]
    fn mount_identity_rejects_missing_and_foreign_mounts() {
        assert!(verify_mount(None, "/mnt/btrfs", "btrfs", "expected").is_err());
        let mut mount = MountInfo {
            target: "/mnt/btrfs".into(),
            source: "/dev/test".into(),
            fstype: "btrfs".into(),
            options: "rw".into(),
            uuid: Some("foreign".into()),
        };
        assert!(verify_mount(Some(&mount), "/mnt/btrfs", "btrfs", "expected").is_err());
        mount.uuid = Some("expected".into());
        assert!(verify_mount(Some(&mount), "/mnt/btrfs", "btrfs", "expected").is_ok());
    }
}
