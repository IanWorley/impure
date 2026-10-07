//! Translates the title's file paths (`game:\...`, `\Device\Cdrom0\...`, `save:\...`) to
//! host paths. Mounts come and go while the title runs (a save package is mounted when the title
//! opens it), and each is read-only, like the disc, or writable, like a save package.
//!
//! The console's file systems are case-insensitive, so each path component is matched against
//! the host directory's entries without regard to case. `.` and `..` are rejected, so a guest
//! path can never reach outside its mount.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Device prefixes are matched case-insensitively; the separator after one is optional.
const SEPARATORS: [char; 2] = ['\\', '/'];
const CURRENT_DIRECTORY: &str = ".";
const PARENT_DIRECTORY: &str = "..";
/// File mask wildcards: any run of characters, and any one character.
const ANY_RUN: char = '*';
const ANY_ONE: char = '?';
/// The DOS spelling of "everything", which also matches names without a dot.
const EVERYTHING_MASK: &str = "*.*";

pub struct DirectoryEntry {
    pub name: String,
    pub is_directory: bool,
    pub size: u64,
    pub modified: SystemTime,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// No mount owns the path's device.
    UnknownDevice,
    /// A component other than the last does not exist, or the path tries to leave its mount.
    PathNotFound,
    /// Everything but the last component exists.
    NameNotFound { parent: PathBuf, name: String },
}

struct Mount {
    /// The device prefix, in lowercase.
    device: String,
    root: PathBuf,
    writable: bool,
}

pub struct Vfs {
    /// Longest device prefix first, so `\Device\Cdrom0` wins over a shorter prefix of it.
    mounts: Vec<Mount>,
}

impl Vfs {
    pub const fn new() -> Self {
        Self { mounts: Vec::new() }
    }

    /// Mounts `root` at `device`, replacing whatever was mounted there.
    pub fn mount(&mut self, device: &str, root: PathBuf, writable: bool) {
        self.unmount(device);
        self.mounts.push(Mount {
            device: device.to_ascii_lowercase(),
            root,
            writable,
        });
        self.mounts
            .sort_by_key(|mount| std::cmp::Reverse(mount.device.len()));
    }

    /// Returns whether anything was mounted at `device`.
    pub fn unmount(&mut self, device: &str) -> bool {
        let device = device.to_ascii_lowercase();
        let before = self.mounts.len();
        self.mounts.retain(|mount| mount.device != device);
        self.mounts.len() != before
    }

    fn mount_for(&self, guest_path: &str) -> Option<&Mount> {
        let lowercase = guest_path.to_ascii_lowercase();
        self.mounts.iter().find(|mount| {
            lowercase.starts_with(mount.device.as_str())
                && lowercase[mount.device.len()..]
                    .chars()
                    .next()
                    .is_none_or(|next| SEPARATORS.contains(&next))
        })
    }

    /// Whether the path is on a writable mount.
    pub fn is_writable(&self, guest_path: &str) -> bool {
        self.mount_for(guest_path)
            .is_some_and(|mount| mount.writable)
    }

    /// The host path for an existing file or directory.
    pub fn resolve(&self, guest_path: &str) -> Result<PathBuf, ResolveError> {
        let mount = self
            .mount_for(guest_path)
            .ok_or(ResolveError::UnknownDevice)?;
        let (device, root) = (&mount.device, &mount.root);

        let components: Vec<&str> = guest_path[device.len()..]
            .split(SEPARATORS)
            .filter(|component| !component.is_empty())
            .collect();
        let mut host = root.clone();
        for (index, component) in components.iter().enumerate() {
            if [CURRENT_DIRECTORY, PARENT_DIRECTORY].contains(component) {
                return Err(ResolveError::PathNotFound);
            }
            match find_entry(&host, component) {
                Some(entry) => host = entry,
                None if index + 1 == components.len() => {
                    return Err(ResolveError::NameNotFound {
                        parent: host,
                        name: component.to_string(),
                    });
                }
                None => return Err(ResolveError::PathNotFound),
            }
        }
        Ok(host)
    }
}

/// A directory's entries, sorted by name ignoring case so enumeration order is stable.
pub fn list_directory(directory: &Path) -> io::Result<Vec<DirectoryEntry>> {
    let mut entries = fs::read_dir(directory)?
        .map(|entry| {
            let entry = entry?;
            let metadata = entry.metadata()?;
            Ok(DirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_directory: metadata.is_dir(),
                size: if metadata.is_dir() { 0 } else { metadata.len() },
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.name.to_ascii_lowercase());
    Ok(entries)
}

/// Whether `name` matches a file mask with `*` and `?` wildcards, ignoring case.
pub fn matches_mask(name: &str, mask: &str) -> bool {
    fn matches(name: &[char], mask: &[char]) -> bool {
        match mask.split_first() {
            None => name.is_empty(),
            Some((&ANY_RUN, rest)) => (0..=name.len()).any(|skip| matches(&name[skip..], rest)),
            Some((&wanted, rest)) => name.split_first().is_some_and(|(&actual, name_rest)| {
                (wanted == ANY_ONE || actual.eq_ignore_ascii_case(&wanted))
                    && matches(name_rest, rest)
            }),
        }
    }
    if mask == EVERYTHING_MASK {
        return true;
    }
    let name: Vec<char> = name.chars().collect();
    let mask: Vec<char> = mask.chars().collect();
    matches(&name, &mask)
}

/// The entry of `directory` named `name`, ignoring case; an exact match wins.
fn find_entry(directory: &Path, name: &str) -> Option<PathBuf> {
    let exact = directory.join(name);
    if exact.exists() {
        return Some(exact);
    }
    fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(name)
        })
        .map(|entry| entry.path())
}

#[cfg(test)]
mod tests {
    use std::process;

    use super::*;

    const DEVICE: &str = "game:";
    const ALIAS: &str = "\\Device\\Cdrom0";

    /// A fresh directory holding `Data/Level.bin`.
    fn disc(test_name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pure-vfs-{}-{test_name}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Data")).unwrap();
        fs::write(root.join("Data").join("Level.bin"), []).unwrap();
        root
    }

    fn vfs(root: &Path) -> Vfs {
        let mut vfs = Vfs::new();
        vfs.mount(DEVICE, root.to_path_buf(), false);
        vfs.mount(ALIAS, root.to_path_buf(), false);
        vfs
    }

    #[test]
    fn paths_resolve_ignoring_case_and_device_spelling() {
        let root = disc("case");
        let expected = root.join("Data").join("Level.bin");
        assert_eq!(
            vfs(&root).resolve("GAME:\\data\\LEVEL.BIN"),
            Ok(expected.clone())
        );
        assert_eq!(
            vfs(&root).resolve("\\device\\cdrom0\\Data\\Level.bin"),
            Ok(expected)
        );
        assert_eq!(vfs(&root).resolve("game:"), Ok(root.clone()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_missing_last_component_names_its_parent() {
        let root = disc("missing");
        assert_eq!(
            vfs(&root).resolve("game:\\Data\\New.sav"),
            Err(ResolveError::NameNotFound {
                parent: root.join("Data"),
                name: "New.sav".to_string()
            })
        );
        assert_eq!(
            vfs(&root).resolve("game:\\Nowhere\\New.sav"),
            Err(ResolveError::PathNotFound)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mounts_can_be_replaced_and_removed_and_know_if_they_are_writable() {
        const SAVE: &str = "save:";
        let root = disc("mounts");
        let mut vfs = vfs(&root);
        vfs.mount(SAVE, root.join("Data"), true);
        assert!(vfs.is_writable("SAVE:\\Level.bin"));
        assert!(!vfs.is_writable("game:\\Data"));
        assert_eq!(
            vfs.resolve("save:\\Level.bin"),
            Ok(root.join("Data").join("Level.bin"))
        );
        assert!(vfs.unmount(SAVE));
        assert_eq!(
            vfs.resolve("save:\\Level.bin"),
            Err(ResolveError::UnknownDevice)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn masks_match_wildcards_ignoring_case() {
        assert!(matches_mask("default.xex", "*"));
        assert!(matches_mask("DEFAULT.XEX", "*.xex"));
        assert!(matches_mask("Level1.bin", "level?.BIN"));
        assert!(matches_mask("README", "*.*"));
        assert!(!matches_mask("default.xex", "*.bin"));
        assert!(!matches_mask("Level10.bin", "Level?.bin"));
    }

    #[test]
    fn listings_are_sorted_ignoring_case() {
        let root = disc("listing");
        fs::write(root.join("b.bin"), [0; 3]).unwrap();
        fs::write(root.join("A.bin"), []).unwrap();
        let entries = list_directory(&root).unwrap();
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["A.bin", "b.bin", "Data"]);
        assert!(entries[2].is_directory);
        assert_eq!(entries[1].size, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paths_cannot_leave_their_mount_or_name_unknown_devices() {
        let root = disc("escape");
        assert_eq!(
            vfs(&root).resolve("game:\\Data\\..\\..\\etc"),
            Err(ResolveError::PathNotFound)
        );
        assert_eq!(
            vfs(&root).resolve("gamer:\\Data"),
            Err(ResolveError::UnknownDevice)
        );
        assert_eq!(
            vfs(&root).resolve("cache:\\x"),
            Err(ResolveError::UnknownDevice)
        );
        fs::remove_dir_all(root).unwrap();
    }
}
