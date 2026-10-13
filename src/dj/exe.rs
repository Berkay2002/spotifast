//! Finding the user's own `claude` and deciding what its environment may contain.
//!
//! Spotifast never touches Claude credentials. The child is the user's installed, unmodified Claude
//! Code, signed in the way the user signed in (their subscription, through `claude login`). Two
//! rules keep it that way and both live here, in pure functions the tests can drive:
//!
//! * a variable that would make Claude Code authenticate or route some other way (an API key, an
//!   auth token, another base URL, a pasted OAuth token, a cloud provider switch) is never passed
//!   on, and when the user's own environment sets one, Claude is refused with a message that names
//!   it: the answer would not be billed to their Claude plan;
//! * the child's environment is an allowlist: what a program needs to run on Windows (paths, the
//!   profile folders, temp, proxy and CA settings), the config directory the user chose and the Git
//!   Bash path Claude Code needs. Everything else is dropped, so a Spotifast started from a Claude Code
//!   terminal does not pass on that session's ids and message socket, and nothing like
//!   `NODE_OPTIONS`, a cloud key or another tool's token reaches the child.

use std::path::{Path, PathBuf};

/// Variables that carry or redirect authentication. Their presence refuses the provider.
const FORCING: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// Switches to another backend (Bedrock, Vertex, Foundry); only a truthy value forces.
const BACKEND_SWITCHES: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// The two `CLAUDE*` variables that are the user's own settings for where things are.
const KEEP: &[&str] = &["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_GIT_BASH_PATH"];

/// Everything else the child may see (names compared case-insensitively, as Windows does).
const ALLOW: &[&str] = &[
    // where programs and the system are
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "TEMP",
    "TMP",
    "OS",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    // the profile
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOME",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "LANG",
    "LC_ALL",
    // a network the user is behind: proxy and certificate settings
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
];

fn truthy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    !(v.is_empty() || v == "0" || v == "false" || v == "no" || v == "off")
}

/// The names (never the values) of the variables in `vars` that force another authentication or
/// backend. Empty when the environment leaves the user's Claude login in charge.
pub fn forcing(vars: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (k, v) in vars {
        let up = k.to_ascii_uppercase();
        let hit = (FORCING.contains(&up.as_str()) && !v.trim().is_empty())
            || (BACKEND_SWITCHES.contains(&up.as_str()) && truthy(v));
        if hit && !out.contains(&up) {
            out.push(up);
        }
    }
    out.sort();
    out
}

/// The sentence shown when `forcing` found something.
pub fn refusal(names: &[String]) -> String {
    format!(
        "Your environment sets {}, which would make Claude Code use that instead of your Claude plan. \
         Spotifast uses only the plan you sign in to with `claude login`, so it will not start Claude while \
         that is set. Remove it (or start Spotifast without it) and try again.",
        names.join(", ")
    )
}

/// What a program needs to run on Linux and macOS (the variables of the profile, temp, the shell
/// and the locale); `LC_*` and `XDG_*` are matched by prefix.
#[cfg(not(windows))]
fn allowed_on_unix(up: &str) -> bool {
    matches!(
        up,
        "USER" | "LOGNAME" | "TMPDIR" | "SHELL" | "LANGUAGE" | "TZ"
    ) || up.starts_with("LC_")
        || up.starts_with("XDG_")
}

#[cfg(windows)]
fn allowed_on_unix(_: &str) -> bool {
    false
}

/// The environment the child gets: the allowlist and the keep-list of `vars`, nothing else.
pub fn child_env(vars: &[(String, String)]) -> Vec<(String, String)> {
    vars.iter()
        .filter(|(k, _)| {
            let up = k.to_ascii_uppercase();
            ALLOW.contains(&up.as_str()) || KEEP.contains(&up.as_str()) || allowed_on_unix(&up)
        })
        .cloned()
        .collect()
}

/// The ways a `--version` answer can end for the real thing: Claude Code prints
/// `2.1.296 (Claude Code)`.
pub fn is_claude_version(out: &str) -> bool {
    out.trim().ends_with("(Claude Code)")
}

/// Whether the user's choice of file could be the CLI: a full local path (no network or device
/// path) to a file named `claude.exe`, `claude` or the npm shim `claude.cmd`. A shim is replaced by
/// the binary it starts. The caller still has to see `--version` answer as Claude Code.
pub fn vet_choice(p: &Path) -> Result<PathBuf, String> {
    let s = p.to_string_lossy();
    // an extended-length path of a local drive (`\\?\C:\...`) is a local path
    let bytes = s.as_bytes();
    let local_ext = s.starts_with(r"\\?\")
        && bytes.len() > 6
        && bytes[4].is_ascii_alphabetic()
        && bytes[5] == b':';
    if !local_ext && (s.starts_with(r"\\") || s.starts_with("//")) {
        return Err("Choose a file on this computer, not a network or device path.".into());
    }
    if !p.is_absolute() {
        return Err("Choose the file itself (its full path), not a relative one.".into());
    }
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let allowed: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd"]
    } else {
        &["claude"]
    };
    if !allowed.contains(&name.as_str()) {
        return Err(format!("The file must be named {}.", allowed.join(" or ")));
    }
    if !p.is_file() {
        return Err("That file does not exist.".into());
    }
    if is_shim(p) {
        return real_exe_behind_shim(p).ok_or_else(|| {
            "That is a launcher script without the claude.exe it starts.".to_string()
        });
    }
    Ok(p.to_path_buf())
}

// ---- finding the executable --------------------------------------------------------------------------

/// Where the CLI might be, best first. `override_path` is the user's own choice (a setting or
/// `SPOTIFAST_CLAUDE`), `path` the PATH directories, `home` the user profile folder.
pub fn candidates(
    override_path: Option<&Path>,
    path: &[PathBuf],
    home: Option<&Path>,
    appdata: Option<&Path>,
) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(p) = override_path {
        v.push(p.to_path_buf());
    }
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd"]
    } else {
        &["claude"]
    };
    for dir in path {
        for n in names {
            v.push(dir.join(n));
        }
    }
    if let Some(h) = home {
        for n in names {
            v.push(h.join(".local").join("bin").join(n));
        }
    }
    if let Some(a) = appdata {
        // the npm global bin folder of a Windows install
        v.push(a.join("npm").join("claude.cmd"));
    }
    v
}

/// An npm shim (`claude.cmd`) next to `node_modules/@anthropic-ai/claude-code`: the real binary
/// the shim starts. Batch files are never run directly (they need a shell and its quoting rules).
pub fn real_exe_behind_shim(shim: &Path) -> Option<PathBuf> {
    let dir = shim.parent()?;
    let pkg = dir
        .join("node_modules")
        .join("@anthropic-ai")
        .join("claude-code");
    for rel in [&["bin", "claude.exe"][..], &["claude.exe"][..]] {
        let mut p = pkg.clone();
        for r in rel {
            p.push(r);
        }
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn is_shim(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("cmd" | "bat" | "ps1")
    )
}

/// The first usable executable among `list`, shims resolved to the binary they start.
pub fn first_usable(list: &[PathBuf]) -> Option<PathBuf> {
    for c in list {
        if !c.is_file() {
            continue;
        }
        if is_shim(c) {
            if let Some(real) = real_exe_behind_shim(c) {
                return Some(real);
            }
            continue;
        }
        return Some(c.clone());
    }
    None
}

/// Resolves from the real environment.
pub fn resolve(override_path: Option<&Path>) -> Option<PathBuf> {
    let path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let env_override = std::env::var_os("SPOTIFAST_CLAUDE").map(PathBuf::from);
    // a choice that is not a plain local `claude` file is ignored, and the search goes on
    let chosen = override_path
        .map(Path::to_path_buf)
        .or(env_override)
        .and_then(|p| vet_choice(&p).ok());
    first_usable(&candidates(
        chosen.as_deref(),
        &path,
        home.as_deref(),
        appdata.as_deref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn auth_forcing_variables_are_found_by_name_only() {
        let v = vars(&[
            ("PATH", "C:\\x"),
            ("anthropic_api_key", "sk-secret"),
            ("ANTHROPIC_BASE_URL", "https://proxy.example"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "tok"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDE_CODE_USE_VERTEX", "0"),
            ("ANTHROPIC_AUTH_TOKEN", ""),
        ]);
        let f = forcing(&v);
        assert_eq!(
            f,
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_BASE_URL",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_USE_BEDROCK"
            ]
        );
        let msg = refusal(&f);
        assert!(
            msg.contains("ANTHROPIC_API_KEY") && !msg.contains("sk-secret"),
            "{msg}"
        );
        assert!(forcing(&vars(&[("PATH", "x"), ("CLAUDE_CONFIG_DIR", "D:\\cfg")])).is_empty());
    }

    #[test]
    fn the_child_environment_is_an_allowlist() {
        let v = vars(&[
            ("PATH", "C:\\x"),
            ("Path", "C:\\y"),
            ("USERPROFILE", "C:\\Users\\a"),
            ("SystemRoot", "C:\\Windows"),
            ("ProgramFiles(x86)", "C:\\Program Files (x86)"),
            ("HTTPS_PROXY", "http://proxy:8080"),
            ("NODE_EXTRA_CA_CERTS", "C:\\ca.pem"),
            ("ANTHROPIC_API_KEY", "sk"),
            ("ANTHROPIC_BASE_URL", "u"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_SESSION_ID", "s"),
            ("CLAUDE_CODE_MESSAGING_TOKEN", "t"),
            ("CLAUDE_EFFORT", "high"),
            ("Claude_Config_Dir", "D:\\cfg"),
            ("CLAUDE_CODE_GIT_BASH_PATH", "C:\\git\\bash.exe"),
            // not claudeish, still not the child's business
            ("NODE_OPTIONS", "--require C:\\evil.js"),
            ("OPENAI_API_KEY", "sk-o"),
            ("GITHUB_TOKEN", "ghp"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("MCP_TOOL_TIMEOUT", "1"),
            ("FAKE_CLAUDE_LOG", "x"),
        ]);
        let e = child_env(&v);
        let names: Vec<&str> = e.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "PATH",
                "Path",
                "USERPROFILE",
                "SystemRoot",
                "ProgramFiles(x86)",
                "HTTPS_PROXY",
                "NODE_EXTRA_CA_CERTS",
                "Claude_Config_Dir",
                "CLAUDE_CODE_GIT_BASH_PATH"
            ]
        );
    }

    /// On Linux and macOS the profile and locale variables of that platform pass too.
    #[cfg(not(windows))]
    #[test]
    fn a_unix_child_keeps_its_profile_temp_shell_and_locale() {
        let v = vars(&[
            ("USER", "a"),
            ("LOGNAME", "a"),
            ("HOME", "/home/a"),
            ("TMPDIR", "/tmp"),
            ("SHELL", "/bin/bash"),
            ("LC_CTYPE", "C.UTF-8"),
            ("XDG_CONFIG_HOME", "/home/a/.config"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("http_proxy", "http://p:1"),
            ("LD_PRELOAD", "/evil.so"),
            ("SSH_AUTH_SOCK", "/s"),
            ("DISPLAY", ":0"),
            ("ANTHROPIC_API_KEY", "sk"),
            ("CLAUDECODE", "1"),
        ]);
        let names: Vec<String> = child_env(&v).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            names,
            [
                "USER",
                "LOGNAME",
                "HOME",
                "TMPDIR",
                "SHELL",
                "LC_CTYPE",
                "XDG_CONFIG_HOME",
                "XDG_RUNTIME_DIR",
                "http_proxy"
            ]
        );
    }

    #[test]
    fn only_a_claude_that_says_so_is_claude() {
        assert!(is_claude_version("2.1.296 (Claude Code)\n"));
        assert!(!is_claude_version("2.1.296"));
        assert!(!is_claude_version("Microsoft Windows [Version 10]"));
        assert!(!is_claude_version("(Claude Code) but not at the end"));
        assert!(!is_claude_version(""));
    }

    #[test]
    fn a_chosen_file_must_be_a_local_file_named_claude() {
        let root =
            std::env::temp_dir().join(format!("spotifast-claude-vet-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });
        std::fs::write(&exe, "MZ").unwrap();
        let other = root.join("notepad.exe");
        std::fs::write(&other, "MZ").unwrap();
        assert_eq!(vet_choice(&exe), Ok(exe.clone()));
        assert!(vet_choice(&other).unwrap_err().contains("must be named"));
        assert!(vet_choice(&root.join("claude.EXE.missing")).is_err());
        assert!(
            vet_choice(Path::new("claude.exe"))
                .unwrap_err()
                .contains("full path")
        );
        assert!(
            vet_choice(Path::new("\\\\server\\share\\claude.exe"))
                .unwrap_err()
                .contains("network")
        );
        assert!(
            vet_choice(Path::new("//server/share/claude.exe"))
                .unwrap_err()
                .contains("network")
        );
        assert!(
            vet_choice(Path::new("\\\\.\\pipe\\claude.exe"))
                .unwrap_err()
                .contains("network")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn candidates_come_in_the_order_override_path_home_npm() {
        let c = candidates(
            Some(Path::new("D:/mine/claude.exe")),
            &[PathBuf::from("C:/bin")],
            Some(Path::new("C:/Users/a")),
            Some(Path::new("C:/Users/a/AppData/Roaming")),
        );
        assert_eq!(c[0], Path::new("D:/mine/claude.exe"));
        assert!(c[1].starts_with("C:/bin"));
        assert!(c.iter().any(|p| p.ends_with(".local/bin/claude.exe")
            || p.ends_with(".local\\bin\\claude.exe")
            || p.ends_with(".local/bin/claude")));
        assert!(c.last().unwrap().to_string_lossy().contains("npm"));
    }

    #[test]
    fn an_npm_shim_resolves_to_the_binary_it_starts_and_a_bare_shim_is_skipped() {
        let root =
            std::env::temp_dir().join(format!("spotifast-claude-shim-{}", std::process::id()));
        let bin = root
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code")
            .join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(root.join("claude.cmd"), "@echo off").unwrap();
        // no exe yet: the shim alone is not usable
        assert_eq!(first_usable(&[root.join("claude.cmd")]), None);
        std::fs::write(bin.join("claude.exe"), "MZ").unwrap();
        assert_eq!(
            first_usable(&[root.join("claude.cmd")]),
            Some(bin.join("claude.exe"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
