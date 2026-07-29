use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Lexically normalize a path without consulting the filesystem.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Resolve symlinks in the existing portion of a path while preserving a
/// possibly-missing suffix. This also follows dangling symlinks, which
/// `canonicalize` alone cannot do but file creation would follow.
pub fn resolve_existing_prefix(path: &Path) -> PathBuf {
    resolve_existing_prefix_inner(&normalize_path(path), 0)
}

fn resolve_existing_prefix_inner(path: &Path, depth: usize) -> PathBuf {
    if depth >= 40 {
        return normalize_path(path);
    }

    let mut probe = path.to_path_buf();
    let mut suffix: Vec<OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&probe) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let Ok(target) = std::fs::read_link(&probe) else {
                    break;
                };
                let target = if target.is_absolute() {
                    target
                } else {
                    probe
                        .parent()
                        .unwrap_or_else(|| Path::new("/"))
                        .join(target)
                };
                let mut resolved = resolve_existing_prefix_inner(&target, depth + 1);
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                return normalize_path(&resolved);
            }
            Ok(_) => {
                let mut resolved =
                    std::fs::canonicalize(&probe).unwrap_or_else(|_| normalize_path(&probe));
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                return normalize_path(&resolved);
            }
            Err(_) => {
                let Some(name) = probe.file_name().map(OsString::from) else {
                    break;
                };
                suffix.push(name);
                if !probe.pop() {
                    break;
                }
            }
        }
    }
    normalize_path(path)
}

pub fn canonical_root(root: &Path) -> PathBuf {
    resolve_existing_prefix(root)
}

pub fn resolve_under_root(root: &Path, base: &Path, input: &Path) -> (PathBuf, bool) {
    let root = canonical_root(root);
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        base.join(input)
    };
    let resolved = resolve_existing_prefix(&joined);
    let outside = !resolved.starts_with(&root);
    (resolved, outside)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolves_existing_symlink_outside_root() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("link")).unwrap();
        let (resolved, escaped) = resolve_under_root(
            workspace.path(),
            workspace.path(),
            Path::new("link/new.txt"),
        );
        assert!(escaped);
        assert_eq!(
            resolved,
            resolve_existing_prefix(&outside.path().join("new.txt"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_dangling_symlink_outside_root() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("future.txt");
        symlink(&target, workspace.path().join("link")).unwrap();
        let (resolved, escaped) =
            resolve_under_root(workspace.path(), workspace.path(), Path::new("link"));
        assert!(escaped);
        assert_eq!(resolved, resolve_existing_prefix(&target));
    }
}
