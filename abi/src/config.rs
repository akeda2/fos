//! Where a machine keeps its configuration files (`hostname`, `settings`, `start`, `fstab`, tape configs, ...): one
//! list shared by the loader-independent readers, both kernels, every tape (`tape_rt::config`) and httpd's block list,
//! so a new name is one change here.
//!
//! A file is looked for in each folder in `FOLDERS` order, then in the root (v3 and FAT volumes have no folders). The
//! first copy found wins. A file that does not exist yet goes to `home`: the first of these folders the volume has,
//! else `DEFAULT`. A machine should have one of them; the order only decides which copy is used when it has several.
//! `etc` and `cfg` were asked for because `settings` is long to type (2026-11-27).

/// The folders searched, in order.
pub const FOLDERS: [&str; 4] = ["etc", "cfg", "settings", "config"];
/// Where new files go on a volume that has none of `FOLDERS`.
pub const DEFAULT: &str = "settings";
/// The longest name `join` takes, and the longest path it makes.
pub const MAX_NAME: usize = 48;
pub const MAX_PATH: usize = 9 + MAX_NAME;

/// A folder and a name joined (`etc/start`), or a bare name (`start`), in a fixed buffer: no allocator needed.
#[derive(Clone, Copy)]
pub struct Path {
    bytes: [u8; MAX_PATH],
    length: usize,
}

impl Path {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.length]
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    /// The folder part (`etc`), empty for a root name.
    pub fn folder(&self) -> &str {
        let text = self.as_str();
        text.rfind('/').map_or("", |at| &text[..at])
    }
}

/// `folder/name`, or `name` alone for an empty folder. `None` if the name is empty or longer than `MAX_NAME`.
pub fn join(folder: &str, name: &str) -> Option<Path> {
    if name.is_empty() || name.len() > MAX_NAME || folder.len() > MAX_PATH - MAX_NAME - 1 {
        return None;
    }
    let mut bytes = [0u8; MAX_PATH];
    let mut length = 0;
    if !folder.is_empty() {
        bytes[..folder.len()].copy_from_slice(folder.as_bytes());
        bytes[folder.len()] = b'/';
        length = folder.len() + 1;
    }
    bytes[length..length + name.len()].copy_from_slice(name.as_bytes());
    Some(Path { bytes, length: length + name.len() })
}

/// Every place `name` may be, in the order to try: each folder of `FOLDERS`, then the root.
pub fn candidates(name: &str) -> impl Iterator<Item = Path> + '_ {
    FOLDERS.iter().copied().chain(core::iter::once("")).filter_map(move |folder| join(folder, name))
}

/// The first place that has `name` (`has_file` answers for a path), or `None`.
pub fn find(name: &str, mut has_file: impl FnMut(&str) -> bool) -> Option<Path> {
    candidates(name).find(|path| has_file(path.as_str()))
}

/// The folder new configuration files go to: the first of `FOLDERS` that `is_folder` says exists, else `DEFAULT`.
pub fn home(mut is_folder: impl FnMut(&str) -> bool) -> &'static str {
    FOLDERS.iter().copied().find(|folder| is_folder(folder)).unwrap_or(DEFAULT)
}

/// True for one of `FOLDERS`, in any case (a FAT volume does not keep case).
pub fn is_folder_name(name: &[u8]) -> bool {
    FOLDERS.iter().any(|folder| name.eq_ignore_ascii_case(folder.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_looked_for_in_every_folder_then_the_root() {
        let all = ["etc/start", "cfg/start", "settings/start", "config/start", "start"];
        assert!(candidates("start").map(|path| path.as_bytes().to_vec()).eq(all.iter().map(|path| path.as_bytes().to_vec())));
    }

    #[test]
    fn the_first_copy_wins() {
        // A machine with settings/ (rack today) and a stray root copy: settings/ is used.
        let found = find("start", |path| path == "settings/start" || path == "start").unwrap();
        assert_eq!(found.as_str(), "settings/start");
        assert_eq!(found.folder(), "settings");
        // Moved to etc/: that copy wins over the old one.
        assert_eq!(find("start", |path| path == "settings/start" || path == "etc/start").unwrap().as_str(), "etc/start");
        // Only the root (v3, a FAT stick).
        let root = find("hostname", |path| path == "hostname").unwrap();
        assert_eq!((root.as_str(), root.folder()), ("hostname", ""));
        assert!(find("debug", |_| false).is_none());
    }

    #[test]
    fn new_files_go_to_the_folder_the_machine_has() {
        assert_eq!(home(|folder| folder == "etc"), "etc");
        assert_eq!(home(|folder| folder == "cfg" || folder == "settings"), "cfg");
        assert_eq!(home(|_| false), "settings");
    }

    #[test]
    fn folder_names_match_in_any_case() {
        assert!(is_folder_name(b"ETC") && is_folder_name(b"Settings") && is_folder_name(b"config"));
        assert!(!is_folder_name(b"www") && !is_folder_name(b"etc2"));
    }

    #[test]
    fn a_name_too_long_or_empty_is_refused() {
        assert!(join("etc", "").is_none());
        assert!(join("etc", &"x".repeat(MAX_NAME + 1)).is_none());
        assert_eq!(join("settings", &"x".repeat(MAX_NAME)).unwrap().as_bytes().len(), 9 + MAX_NAME);
    }
}
