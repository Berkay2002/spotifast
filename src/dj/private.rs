//! The child's private folder and the config file in it.
//!
//! The folder is the child's working directory and holds `mcp.json`, which carries the bearer token
//! of Spotifast's tool server. On a shared temp directory (Linux, macOS, or a Windows `TEMP` such as
//! `C:\Temp`) a predictable name or a default-permission file would let another local user read the
//! token or plant a link in its place, so the name is 128 random bits, nothing that already exists is
//! ever reused, and only the owner can enter the folder or read the file: mode 0700/0600 on unix, a
//! protected owner-only access list on Windows that the file inherits.

use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

/// How often a name that is already taken is replaced by another random one.
const TRIES: usize = 8;

/// Creates exactly `dir`, which must not exist (`create_dir`, never `create_dir_all`: a planted
/// directory or link at that path is an error). Only the owner can enter it.
#[cfg(not(windows))]
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

/// Creates exactly `dir`, which must not exist, with an access list that is set at creation (no
/// moment with the parent's inherited one), is protected from that parent, and grants the owner
/// alone full access, inherited by everything made inside it.
#[cfg(windows)]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    // D:P protected DACL, one allow ACE: OI|CI inherited by files and folders, FA full access,
    // OW the object's owner (the user Spotifast runs as)
    let sddl: Vec<u16> = "D:P(A;OICI;FA;;;OW)\0".encode_utf16().collect();
    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: both strings are NUL-terminated and outlive the calls; `sd` is freed below
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let r = if CreateDirectoryW(path.as_ptr(), &sa) == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        LocalFree(sd);
        r
    }
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

/// Removes the folders an earlier run left behind (it was killed before it could clean up): real
/// directories named exactly like the ones `private_dir_in` makes, untouched for `age`. A folder
/// a running child works in was made at its start, so `age` is far longer than a turn.
// ponytail: the age is a day. A child that has run longer keeps its folder on Windows (a working
// directory cannot be deleted) and may lose it on unix, where it has read its config long since.
pub fn remove_stale(root: &Path, age: std::time::Duration, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let ours = name
            .to_str()
            .and_then(|n| n.strip_prefix("spotifast-claude-"))
            .is_some_and(|hex| hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()));
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let old = meta
            .modified()
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|d| d >= age);
        if ours && meta.is_dir() && old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Writes `bytes` to a file that must not exist yet (`create_new`: a planted file or link is an
/// error, not something to write through). On unix only the owner can read it; on Windows it
/// takes the owner-only access list of the folder `create_private_dir` made.
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
    fn only_old_folders_of_this_kind_are_removed() {
        use std::time::{Duration, SystemTime};
        let root = scratch("stale");
        let mine = private_dir_in(&root).unwrap();
        let other = root.join("spotifast-claude-planted");
        std::fs::create_dir(&other).unwrap();
        let file = root.join(format!("spotifast-claude-{}", "a".repeat(32)));
        std::fs::write(&file, b"x").unwrap();
        let day = Duration::from_secs(24 * 3600);
        // nothing is old yet
        remove_stale(&root, day, SystemTime::now());
        assert!(mine.is_dir());
        // two days on: the folder of this kind goes, the others stay
        remove_stale(&root, day, SystemTime::now() + day * 2);
        assert!(!mine.exists());
        assert!(other.is_dir());
        assert!(file.is_file());
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

    #[cfg(windows)]
    #[test]
    fn only_the_owner_can_enter_the_folder_or_read_the_file() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
        fn dacl(p: &Path) -> String {
            let path: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let mut s: *mut u16 = std::ptr::null_mut();
            let mut len = 0u32;
            // SAFETY: NUL-terminated path; both returned buffers are freed with LocalFree
            unsafe {
                let e = GetNamedSecurityInfoW(
                    path.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut sd,
                );
                assert_eq!(e, 0, "{}", p.display());
                assert_ne!(
                    ConvertSecurityDescriptorToStringSecurityDescriptorW(
                        sd,
                        SDDL_REVISION_1,
                        DACL_SECURITY_INFORMATION,
                        &mut s,
                        &mut len,
                    ),
                    0
                );
                let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len as usize));
                LocalFree(s.cast());
                LocalFree(sd);
                out.trim_end_matches('\0').to_string()
            }
        }
        let root = scratch("acl");
        let dir = private_dir_in(&root).unwrap();
        // protected from the parent's list, and the owner is the only one in it
        assert_eq!(dacl(&dir), "D:P(A;OICI;FA;;;OW)");
        let cfg = dir.join("mcp.json");
        write_new_private(&cfg, b"{}").unwrap();
        assert_eq!(dacl(&cfg), "D:(A;ID;FA;;;OW)");
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
