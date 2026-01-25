use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use zb_core::Error;

pub struct Linker {
    bin_dir: PathBuf,
    opt_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LinkedFile {
    pub link_path: PathBuf,
    pub target_path: PathBuf,
}

impl Linker {
    pub fn new(prefix: &Path) -> io::Result<Self> {
        let bin_dir = prefix.join("bin");
        let opt_dir = prefix.join("opt");
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&opt_dir)?;
        Ok(Self { bin_dir, opt_dir })
    }

    /// Link all executables from a keg's bin directory and create opt symlink.
    /// Returns the list of created links.
    /// Errors on conflict (existing file/link that doesn't point to our keg).
    pub fn link_keg(&self, keg_path: &Path) -> Result<Vec<LinkedFile>, Error> {
        // Create opt symlink: /opt/homebrew/opt/<name> -> /opt/homebrew/Cellar/<name>/<version>
        self.link_opt(keg_path)?;

        let keg_bin = keg_path.join("bin");

        if !keg_bin.exists() {
            return Ok(Vec::new());
        }

        let mut linked = Vec::new();

        for entry in fs::read_dir(&keg_bin).map_err(|e| Error::StoreCorruption {
            message: format!("failed to read keg bin directory: {e}"),
        })? {
            let entry = entry.map_err(|e| Error::StoreCorruption {
                message: format!("failed to read directory entry: {e}"),
            })?;

            let file_name = entry.file_name();
            let target_path = entry.path();
            let link_path = self.bin_dir.join(&file_name);

            // Check for conflicts
            if link_path.exists() || link_path.symlink_metadata().is_ok() {
                // Check if it's our own link (compare canonical paths to handle relative symlinks)
                if let Ok(existing_target) = fs::read_link(&link_path) {
                    // Resolve relative symlinks by joining with the link's parent directory
                    let resolved_existing = if existing_target.is_relative() {
                        link_path.parent().unwrap_or(Path::new("")).join(&existing_target)
                    } else {
                        existing_target
                    };

                    // Canonicalize both to compare actual filesystem locations
                    let existing_canonical = fs::canonicalize(&resolved_existing).ok();
                    let target_canonical = fs::canonicalize(&target_path).ok();

                    if existing_canonical.is_some() && existing_canonical == target_canonical {
                        // Already linked to us, skip
                        linked.push(LinkedFile {
                            link_path,
                            target_path,
                        });
                        continue;
                    }

                    // If existing symlink is broken (target doesn't exist), remove it
                    if existing_canonical.is_none() {
                        fs::remove_file(&link_path).map_err(|e| Error::StoreCorruption {
                            message: format!("failed to remove broken symlink: {e}"),
                        })?;
                        // Fall through to create new symlink below
                    } else {
                        return Err(Error::LinkConflict { path: link_path });
                    }
                } else {
                    // Not a symlink - it's a real file, conflict
                    return Err(Error::LinkConflict { path: link_path });
                }
            }

            // Create symlink
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target_path, &link_path).map_err(|e| {
                Error::StoreCorruption {
                    message: format!("failed to create symlink: {e}"),
                }
            })?;

            #[cfg(not(unix))]
            return Err(Error::StoreCorruption {
                message: "symlinks not supported on this platform".to_string(),
            });

            linked.push(LinkedFile {
                link_path,
                target_path,
            });
        }

        Ok(linked)
    }

    /// Unlink all executables that point to the given keg and remove opt symlink.
    pub fn unlink_keg(&self, keg_path: &Path) -> Result<Vec<PathBuf>, Error> {
        // Remove opt symlink
        self.unlink_opt(keg_path)?;

        let keg_bin = keg_path.join("bin");

        if !keg_bin.exists() {
            return Ok(Vec::new());
        }

        let mut unlinked = Vec::new();

        for entry in fs::read_dir(&keg_bin).map_err(|e| Error::StoreCorruption {
            message: format!("failed to read keg bin directory: {e}"),
        })? {
            let entry = entry.map_err(|e| Error::StoreCorruption {
                message: format!("failed to read directory entry: {e}"),
            })?;

            let file_name = entry.file_name();
            let target_path = entry.path();
            let link_path = self.bin_dir.join(&file_name);

            // Only remove if it's a symlink pointing to our keg
            if let Ok(existing_target) = fs::read_link(&link_path) {
                // Resolve relative symlinks by joining with the link's parent directory
                let resolved_existing = if existing_target.is_relative() {
                    link_path.parent().unwrap_or(Path::new("")).join(&existing_target)
                } else {
                    existing_target
                };

                // Canonicalize both to compare actual filesystem locations
                let existing_canonical = fs::canonicalize(&resolved_existing).ok();
                let target_canonical = fs::canonicalize(&target_path).ok();

                if existing_canonical.is_some() && existing_canonical == target_canonical {
                    fs::remove_file(&link_path).map_err(|e| Error::StoreCorruption {
                        message: format!("failed to remove symlink: {e}"),
                    })?;
                    unlinked.push(link_path);
                }
            }
        }

        Ok(unlinked)
    }

    /// Remove opt symlink if it points to the given keg
    fn unlink_opt(&self, keg_path: &Path) -> Result<(), Error> {
        let name = keg_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());

        if let Some(name) = name {
            let opt_link = self.opt_dir.join(name);
            if let Ok(target) = fs::read_link(&opt_link) {
                // Resolve relative symlinks
                let resolved = if target.is_relative() {
                    opt_link.parent().unwrap_or(Path::new("")).join(&target)
                } else {
                    target
                };
                // Compare canonical paths
                let resolved_canonical = fs::canonicalize(&resolved).ok();
                let keg_canonical = fs::canonicalize(keg_path).ok();
                if resolved_canonical.is_some() && resolved_canonical == keg_canonical {
                    let _ = fs::remove_file(&opt_link);
                }
            }
        }
        Ok(())
    }

    /// Create opt symlink: /opt/homebrew/opt/<name> -> keg_path
    fn link_opt(&self, keg_path: &Path) -> Result<(), Error> {
        // Extract formula name from keg_path (e.g., /opt/homebrew/Cellar/libtool/2.5.4 -> libtool)
        let name = keg_path
            .parent() // Cellar/<name>
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .ok_or_else(|| Error::StoreCorruption {
                message: "could not determine formula name from keg path".to_string(),
            })?;

        let opt_link = self.opt_dir.join(name);

        // Remove existing symlink if it points somewhere else
        if opt_link.symlink_metadata().is_ok() {
            if let Ok(target) = fs::read_link(&opt_link) {
                // Resolve relative symlinks
                let resolved = if target.is_relative() {
                    opt_link.parent().unwrap_or(Path::new("")).join(&target)
                } else {
                    target
                };
                // Compare canonical paths
                let resolved_canonical = fs::canonicalize(&resolved).ok();
                let keg_canonical = fs::canonicalize(keg_path).ok();
                if resolved_canonical.is_some() && resolved_canonical == keg_canonical {
                    return Ok(()); // Already correct
                }
            }
            fs::remove_file(&opt_link).map_err(|e| Error::StoreCorruption {
                message: format!("failed to remove old opt symlink: {e}"),
            })?;
        }

        // Create symlink
        #[cfg(unix)]
        std::os::unix::fs::symlink(keg_path, &opt_link).map_err(|e| Error::StoreCorruption {
            message: format!("failed to create opt symlink: {e}"),
        })?;

        Ok(())
    }

    /// Check if a keg is currently linked.
    pub fn is_linked(&self, keg_path: &Path) -> bool {
        let keg_bin = keg_path.join("bin");

        if !keg_bin.exists() {
            return false;
        }

        if let Ok(entries) = fs::read_dir(&keg_bin) {
            for entry in entries.flatten() {
                let target_path = entry.path();
                let link_path = self.bin_dir.join(entry.file_name());

                if let Ok(existing_target) = fs::read_link(&link_path) {
                    // Resolve relative symlinks by joining with the link's parent directory
                    let resolved_existing = if existing_target.is_relative() {
                        link_path.parent().unwrap_or(Path::new("")).join(&existing_target)
                    } else {
                        existing_target
                    };

                    // Canonicalize both to compare actual filesystem locations
                    let existing_canonical = fs::canonicalize(&resolved_existing).ok();
                    let target_canonical = fs::canonicalize(&target_path).ok();

                    if existing_canonical.is_some() && existing_canonical == target_canonical {
                        return true;
                    }
                }
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn setup_keg(tmp: &TempDir, name: &str) -> PathBuf {
        let keg_path = tmp.path().join("cellar").join(name).join("1.0.0");
        fs::create_dir_all(keg_path.join("bin")).unwrap();

        // Create executable
        fs::write(keg_path.join("bin").join(name), b"#!/bin/sh\necho hi").unwrap();
        let mut perms = fs::metadata(keg_path.join("bin").join(name))
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(keg_path.join("bin").join(name), perms).unwrap();

        keg_path
    }

    #[test]
    fn links_executables_to_bin() {
        let tmp = TempDir::new().unwrap();
        let keg_path = setup_keg(&tmp, "foo");

        let prefix = tmp.path().join("homebrew");
        let linker = Linker::new(&prefix).unwrap();

        let linked = linker.link_keg(&keg_path).unwrap();

        assert_eq!(linked.len(), 1);
        assert!(linked[0].link_path.ends_with("bin/foo"));

        // Verify symlink exists and points correctly
        let link_target = fs::read_link(&linked[0].link_path).unwrap();
        assert_eq!(link_target, keg_path.join("bin/foo"));
    }

    #[test]
    fn conflict_returns_error() {
        let tmp = TempDir::new().unwrap();
        let keg1 = setup_keg(&tmp, "foo");

        // Create another keg with same executable name
        let keg2 = tmp.path().join("cellar/bar/1.0.0");
        fs::create_dir_all(keg2.join("bin")).unwrap();
        fs::write(keg2.join("bin/foo"), b"#!/bin/sh\necho bar").unwrap();

        let prefix = tmp.path().join("homebrew");
        let linker = Linker::new(&prefix).unwrap();

        // Link first keg
        linker.link_keg(&keg1).unwrap();

        // Second keg should fail with conflict
        let result = linker.link_keg(&keg2);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(matches!(err, Error::LinkConflict { .. }));
    }

    #[test]
    fn unlink_removes_symlinks() {
        let tmp = TempDir::new().unwrap();
        let keg_path = setup_keg(&tmp, "foo");

        let prefix = tmp.path().join("homebrew");
        let linker = Linker::new(&prefix).unwrap();

        // Link
        let linked = linker.link_keg(&keg_path).unwrap();
        assert!(linked[0].link_path.exists());

        // Unlink
        let unlinked = linker.unlink_keg(&keg_path).unwrap();
        assert_eq!(unlinked.len(), 1);
        assert!(!linked[0].link_path.exists());
    }

    #[test]
    fn is_linked_returns_correct_state() {
        let tmp = TempDir::new().unwrap();
        let keg_path = setup_keg(&tmp, "foo");

        let prefix = tmp.path().join("homebrew");
        let linker = Linker::new(&prefix).unwrap();

        assert!(!linker.is_linked(&keg_path));

        linker.link_keg(&keg_path).unwrap();
        assert!(linker.is_linked(&keg_path));

        linker.unlink_keg(&keg_path).unwrap();
        assert!(!linker.is_linked(&keg_path));
    }

    #[test]
    fn relinking_same_keg_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let keg_path = setup_keg(&tmp, "foo");

        let prefix = tmp.path().join("homebrew");
        let linker = Linker::new(&prefix).unwrap();

        // Link twice
        let linked1 = linker.link_keg(&keg_path).unwrap();
        let linked2 = linker.link_keg(&keg_path).unwrap();

        assert_eq!(linked1.len(), linked2.len());
    }

    #[test]
    fn keg_without_bin_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let keg_path = tmp.path().join("cellar/empty/1.0.0");
        fs::create_dir_all(&keg_path).unwrap();
        // No bin directory

        let prefix = tmp.path().join("homebrew");
        let linker = Linker::new(&prefix).unwrap();

        let linked = linker.link_keg(&keg_path).unwrap();
        assert!(linked.is_empty());
    }
}
