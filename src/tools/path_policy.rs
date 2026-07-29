use std::path::{Path, PathBuf};

use crate::config::OutsideWorkspace;

#[derive(Debug, Clone)]
pub struct PathResolution {
    pub normalized: PathBuf,
    pub outside_workspace: bool,
}

#[derive(Debug, Clone)]
pub struct PathPolicy {
    root: PathBuf,
    outside: OutsideWorkspace,
}

impl PathPolicy {
    pub fn new(root: &str, outside: OutsideWorkspace) -> Self {
        Self {
            root: crate::path_security::canonical_root(Path::new(root)),
            outside,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve(&self, path: &str) -> PathResolution {
        let p = Path::new(path);
        let (normalized, outside_workspace) =
            crate::path_security::resolve_under_root(&self.root, &self.root, p);
        PathResolution {
            normalized,
            outside_workspace,
        }
    }

    pub fn require_prompt_for_path(&self, path: &str) -> bool {
        self.outside == OutsideWorkspace::Prompt && self.resolve(path).outside_workspace
    }

    pub fn deny_path(&self, path: &str) -> Option<String> {
        let resolved = self.resolve(path);
        if self.outside == OutsideWorkspace::Deny && resolved.outside_workspace {
            Some(format!(
                "Path is outside workspace root: {} (root: {})",
                resolved.normalized.display(),
                self.root.display()
            ))
        } else {
            None
        }
    }

    pub fn require_prompt_for_cwd(&self) -> bool {
        self.outside == OutsideWorkspace::Prompt && !self.cwd_is_inside()
    }

    pub fn deny_cwd(&self) -> Option<String> {
        if self.outside == OutsideWorkspace::Deny && !self.cwd_is_inside() {
            Some(format!(
                "Current directory is outside workspace root: {}",
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .display()
            ))
        } else {
            None
        }
    }

    fn cwd_is_inside(&self) -> bool {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        crate::path_security::resolve_existing_prefix(&cwd).starts_with(&self.root)
    }
}

impl Default for PathPolicy {
    fn default() -> Self {
        Self::new(".", OutsideWorkspace::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_relative_path_under_workspace() {
        let cwd = std::env::current_dir().unwrap();
        let policy = PathPolicy::new(cwd.to_str().unwrap(), OutsideWorkspace::Prompt);
        let resolved = policy.resolve("src/../Cargo.toml");
        assert!(resolved.normalized.ends_with("Cargo.toml"));
        assert!(!resolved.outside_workspace);
    }

    #[test]
    fn detects_outside_workspace() {
        let policy = PathPolicy::new("/tmp/workspace", OutsideWorkspace::Prompt);
        assert!(policy.resolve("/tmp/other/file.txt").outside_workspace);
        assert!(policy.require_prompt_for_path("/tmp/other/file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn detects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("link")).unwrap();
        let policy = PathPolicy::new(workspace.path().to_str().unwrap(), OutsideWorkspace::Deny);
        assert!(policy.resolve("link/secret.txt").outside_workspace);
        assert!(policy.deny_path("link/secret.txt").is_some());
    }
}
