//! Resolve the absolute path of the real claude binary on this machine.
//!
//! Once our shim is installed as `claude.exe` ahead of the user's existing
//! claude on PATH, neither the manager nor the shim can rely on bare
//! `claude` resolving to the real thing — it'd resolve to the shim and
//! recurse. Both call sites use this module to find the real one.
//!
//! Strategy: read a cached path from `%APPDATA%\ClaudeManager\
//! real_claude.txt`; if missing or invalid, run `where.exe claude` and
//! pick the first result whose absolute path isn't `skip_exe`. Cache the
//! winner for next time so the shim doesn't pay a process-spawn cost on
//! every invocation.

use std::path::{Path, PathBuf};
use std::process::Command;

const CACHE_FILENAME: &str = "real_claude.txt";
const APP_DIR: &str = "ClaudeManager";

/// Resolve the real claude path. `skip_exe` is filtered out of the
/// `where.exe` results — the shim passes its own `current_exe()` here so
/// it never points back at itself.
pub fn resolve(skip_exe: Option<&Path>) -> Option<PathBuf> {
    if let Some(cached) = read_cache() {
        if cached.is_file() && !same_path_ci(&cached, skip_exe) {
            return Some(cached);
        }
    }
    let found = run_where(skip_exe)?;
    let _ = write_cache(&found);
    Some(found)
}

/// Force-rediscover regardless of cache. Used when the cached path
/// becomes invalid (file moved, npm reinstalled to a different prefix).
#[allow(dead_code)]
pub fn rediscover(skip_exe: Option<&Path>) -> Option<PathBuf> {
    let found = run_where(skip_exe)?;
    let _ = write_cache(&found);
    Some(found)
}

fn cache_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join(APP_DIR).join(CACHE_FILENAME))
}

fn read_cache() -> Option<PathBuf> {
    let path = cache_path()?;
    let s = std::fs::read_to_string(path).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn write_cache(path: &Path) -> std::io::Result<()> {
    let Some(cache) = cache_path() else {
        return Ok(());
    };
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(cache, path.to_string_lossy().as_bytes())
}

fn run_where(skip_exe: Option<&Path>) -> Option<PathBuf> {
    let output = Command::new("where.exe").arg("claude").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let p = PathBuf::from(line.trim());
        if p.as_os_str().is_empty() {
            continue;
        }
        if !p.is_file() {
            continue;
        }
        if same_path_ci(&p, skip_exe) {
            continue;
        }
        return Some(p);
    }
    None
}

fn same_path_ci(a: &Path, b: Option<&Path>) -> bool {
    let Some(b) = b else {
        return false;
    };
    a.as_os_str().to_string_lossy().to_lowercase()
        == b.as_os_str().to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_ci_handles_case_differences() {
        // Windows is case-insensitive about drive letters and paths;
        // `where.exe` may return one casing while `current_exe` returns
        // another. If we don't normalize, the shim could match itself
        // and decide there's no real claude.
        let a = PathBuf::from(r"C:\Users\Robin\.local\bin\claude.exe");
        let b = PathBuf::from(r"c:\users\robin\.LOCAL\bin\CLAUDE.EXE");
        assert!(same_path_ci(&a, Some(&b)));
    }

    #[test]
    fn same_path_ci_distinguishes_actual_different_paths() {
        let a = PathBuf::from(r"C:\foo\claude.exe");
        let b = PathBuf::from(r"C:\bar\claude.exe");
        assert!(!same_path_ci(&a, Some(&b)));
    }

    #[test]
    fn same_path_ci_returns_false_for_none() {
        let a = PathBuf::from(r"C:\foo\claude.exe");
        assert!(!same_path_ci(&a, None));
    }
}
