//! Runtime path resolution. Nothing here bakes a build machine's filesystem
//! layout into the binary.

use std::path::PathBuf;

const LIBRARY_ENV: &str = "SYMTROPY_LIBRARY";

/// Per-user config directory.
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir)
}

pub fn state_path() -> PathBuf {
    config_dir().join("symtropy_state.json")
}

pub fn bookmarks_path() -> PathBuf {
    config_dir().join("symtropy_bookmarks.json")
}

pub fn cache_dir() -> PathBuf {
    // An empty XDG_CACHE_HOME (common in shell configs) must not turn the
    // cache into a relative path in whatever directory the app was launched from.
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir);
    base.join("symtropy")
}

/// Explicit library override from the environment, if set.
pub fn env_library_dir() -> Option<PathBuf> {
    std::env::var_os(LIBRARY_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Locate the default image library without relying on `CARGO_MANIFEST_DIR`.
///
/// Resolution order:
///   1. `SYMTROPY_LIBRARY` environment variable
///   2. `database/References` next to the executable or any of its ancestors
///      (covers both an installed layout and `target/<profile>/symtropy-ui`)
///   3. `database/References` under the current working directory
///   4. the current working directory, or home as a last resort
pub fn default_library_dir() -> PathBuf {
    if let Some(dir) = env_library_dir() {
        if dir.is_dir() {
            return dir;
        }
        eprintln!("paths: {} is not a directory, ignoring", dir.display());
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut ancestor = exe.parent();
        while let Some(dir) = ancestor {
            let candidate = dir.join("database/References");
            if candidate.is_dir() {
                return candidate;
            }
            ancestor = dir.parent();
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("database/References");
        if candidate.is_dir() {
            return candidate;
        }
        return cwd;
    }

    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_is_absolute_with_empty_xdg_cache_home() {
        let old = std::env::var_os("XDG_CACHE_HOME");
        std::env::set_var("XDG_CACHE_HOME", "");
        let dir = cache_dir();
        match old {
            Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
        assert!(dir.is_absolute(), "cache dir was relative: {}", dir.display());
        assert!(dir.ends_with("symtropy"));
    }
}
