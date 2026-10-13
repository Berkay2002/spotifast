//! The child's private folder and the config file in it.
//!
//! The folder is the child's working directory and holds `mcp.json`, which carries the bearer token
//! of Spotifast's tool server. On a shared temp directory (Linux, macOS) a predictable name or a
//! default-permission file would let another local user read the token or plant a link in its place,
//! so the name is 128 random bits, nothing that already exists is ever reused, and only the owner
//! can enter the folder or read the file.

use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

/// How often a name that is already taken is replaced by another random one.
const TRIES: usize = 8;

/// Creates exactly `dir`, which must not exist (`create_dir`, never `create_dir_all`: a planted
/// directory or link at that path is an error). On unix only the owner can enter it.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[allow(unused_mut)] // only the unix mode setter needs `mut`
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// A new private folder `spotifast-claude-<32 hex digits>` in `root`.
pub fn private_dir_in(root: &Path) -> io::Result<PathBuf> {
    let mut taken = None;
    for _ in 0..TRIES {
        let mut r = [0u8; 16];
        rand::Rng::fill(&mut rand::rng(), &mut r);
        let name: String = r.iter().map(|x| format!("{x:02x}")).collect();
        let dir = root.join(format!("spotifast-claude-{name}"));
        match create_private_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => taken = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(taken.unwrap_or_else(|| io::Error::other("no free folder name")))
}

/// Writes `bytes` to a file that must not exist yet (`create_new`: a planted file or link is an
/// error, not something to write through). On unix only the owner can read it.
pub fn write_new_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("spotifast-private-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_folder_name_is_128_random_bits_and_never_repeats() {
        let root = scratch("names");
        let a = private_dir_in(&root).unwrap();
        let b = private_dir_in(&root).unwrap();
        assert_ne!(a, b);
        for d in [&a, &b] {
            assert!(d.is_dir());
            assert_eq!(d.parent().unwrap(), root);
            let name = d.file_name().unwrap().to_str().unwrap();
            let hex = name.strip_prefix("spotifast-claude-").expect(name);
            assert_eq!(hex.len(), 32, "{name}");
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_existing_path_is_never_reused() {
        let root = scratch("exists");
        let planted = root.join("spotifast-claude-planted");
        std::fs::create_dir(&planted).unwrap();
        let e = create_private_dir(&planted).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::AlreadyExists);
        // a file in its place is refused too
        let file = root.join("spotifast-claude-file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            create_private_dir(&file).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        // and a missing parent is an error, not a trail of created folders
        assert!(create_private_dir(&root.join("nope").join("deeper")).is_err());
        assert!(!root.join("nope").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_planted_config_file_is_refused_and_left_alone() {
        let root = scratch("planted");
        let cfg = root.join("mcp.json");
        std::fs::write(&cfg, b"planted").unwrap();
        let e = write_new_private(&cfg, b"{\"token\":\"secret\"}").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&cfg).unwrap(), b"planted");
        // a fresh path is written whole
        let fresh = root.join("fresh.json");
        write_new_private(&fresh, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"{\"a\":1}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_can_enter_the_folder_or_read_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("modes");
        let dir = private_dir_in(&root).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        let cfg = dir.join("mcp.json");
        write_new_private(&cfg, b"{}").unwrap();
        assert_eq!(mode(&cfg), 0o600);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_planted_at_the_config_path_is_not_written_through() {
        let root = scratch("link");
        let target = root.join("victim");
        std::fs::write(&target, b"keep").unwrap();
        let cfg = root.join("mcp.json");
        std::os::unix::fs::symlink(&target, &cfg).unwrap();
        assert!(write_new_private(&cfg, b"token").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        // a dangling link is refused as well
        let dangling = root.join("dangling.json");
        std::os::unix::fs::symlink(root.join("missing"), &dangling).unwrap();
        assert!(write_new_private(&dangling, b"token").is_err());
        assert!(!root.join("missing").exists());
        // and a link in place of the folder
        let link = root.join("spotifast-claude-link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert_eq!(
            create_private_dir(&link).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
