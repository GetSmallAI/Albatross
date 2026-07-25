//! One-time migration of a pre-rename directory to its current name, shared by
//! the config directory (`~/.config/small-harness` → `albatross`) and the
//! per-project scratch directory (`.small-harness` → `.albatross`).

use std::fs;
use std::path::PathBuf;

/// What a migration actually did, so callers can report it accurately.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// Nothing existed at the new path, so the whole directory was renamed.
    Moved { from: PathBuf, to: PathBuf },
    /// Both paths existed, so the entries the new path lacked were carried over.
    /// `legacy_removed` is false when entries stayed behind because moving them
    /// would have overwritten something.
    Merged {
        from: PathBuf,
        to: PathBuf,
        entries: usize,
        legacy_removed: bool,
    },
}

impl Migration {
    /// One-line startup note. Worth printing unprompted: the user should know
    /// their credentials or notes changed location.
    pub fn describe(&self) -> String {
        match self {
            Migration::Moved { from, to } => {
                format!("moved {} → {}", from.display(), to.display())
            }
            Migration::Merged {
                from,
                to,
                entries,
                legacy_removed,
            } => {
                let noun = if *entries == 1 { "entry" } else { "entries" };
                let mut msg = format!(
                    "carried {entries} {noun} from {} into {}",
                    from.display(),
                    to.display()
                );
                if !legacy_removed {
                    msg.push_str(" (kept the rest, already present at the new path)");
                }
                msg
            }
        }
    }
}

/// Rename `legacy` to `current`, or merge into `current` when both exist.
///
/// The merge case is the important one: the new directory is easy to create by
/// accident — one run of a renamed build, or a cache file written on a code path
/// that only meant to read — and an all-or-nothing skip would then strand real
/// credentials or hand-written notes in the old directory permanently.
///
/// Never overwrites. An entry already present at the new path is left where it
/// is, and its legacy copy stays behind rather than being deleted, so nothing is
/// lost either way. Subdirectories move whole instead of merging recursively,
/// which keeps that guarantee easy to reason about.
///
/// Best-effort: a failed move reports whatever did succeed rather than failing
/// the caller's startup.
pub fn migrate_dir(legacy: PathBuf, current: PathBuf) -> Option<Migration> {
    if !legacy.is_dir() {
        return None;
    }
    if !current.exists() {
        fs::rename(&legacy, &current).ok()?;
        return Some(Migration::Moved {
            from: legacy,
            to: current,
        });
    }

    let mut entries = 0;
    for entry in fs::read_dir(&legacy).ok()?.flatten() {
        let target = current.join(entry.file_name());
        if target.exists() {
            continue;
        }
        if fs::rename(entry.path(), &target).is_ok() {
            entries += 1;
        }
    }
    // Succeeds only once the legacy directory is empty, i.e. everything was
    // carried over. Tidying it away silently is fine; there is nothing left.
    let legacy_removed = fs::remove_dir(&legacy).is_ok();
    if entries == 0 {
        return None;
    }
    Some(Migration::Merged {
        from: legacy,
        to: current,
        entries,
        legacy_removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &PathBuf, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn renames_whole_directory_when_target_is_absent() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("old");
        let current = base.path().join("new");
        write(&legacy.join("auth.json"), "creds");

        let result = migrate_dir(legacy.clone(), current.clone()).expect("should migrate");
        assert_eq!(
            result,
            Migration::Moved {
                from: legacy.clone(),
                to: current.clone()
            }
        );
        assert!(!legacy.exists());
        assert_eq!(
            fs::read_to_string(current.join("auth.json")).unwrap(),
            "creds"
        );
    }

    #[test]
    fn carries_credentials_across_when_a_stray_file_already_exists() {
        // The regression this function exists for: a cache file at the new path
        // must not strand the login in the old directory.
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("old");
        let current = base.path().join("new");
        write(&legacy.join("auth.json"), "creds");
        write(&current.join("grok-client-version"), "0.2.93\n");

        let result = migrate_dir(legacy.clone(), current.clone()).expect("should migrate");
        assert_eq!(
            result,
            Migration::Merged {
                from: legacy.clone(),
                to: current.clone(),
                entries: 1,
                legacy_removed: true
            }
        );
        assert_eq!(
            fs::read_to_string(current.join("auth.json")).unwrap(),
            "creds"
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn never_overwrites_an_entry_present_at_the_new_path() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("old");
        let current = base.path().join("new");
        write(&legacy.join("auth.json"), "stale");
        write(&legacy.join("rubric.md"), "hand-written");
        write(&current.join("auth.json"), "live");

        let result = migrate_dir(legacy.clone(), current.clone()).expect("should migrate");
        assert_eq!(
            result,
            Migration::Merged {
                from: legacy.clone(),
                to: current.clone(),
                entries: 1,
                legacy_removed: false
            }
        );
        // Live credential untouched, unique file carried, stale copy preserved.
        assert_eq!(
            fs::read_to_string(current.join("auth.json")).unwrap(),
            "live"
        );
        assert_eq!(
            fs::read_to_string(current.join("rubric.md")).unwrap(),
            "hand-written"
        );
        assert_eq!(
            fs::read_to_string(legacy.join("auth.json")).unwrap(),
            "stale"
        );
    }

    #[test]
    fn moves_subdirectories_whole() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("old");
        let current = base.path().join("new");
        write(&legacy.join("sessions").join("a.json"), "session");
        write(&current.join("grok-client-version"), "0.2.93\n");

        migrate_dir(legacy, current.clone()).expect("should migrate");
        assert_eq!(
            fs::read_to_string(current.join("sessions").join("a.json")).unwrap(),
            "session"
        );
    }

    #[test]
    fn reports_nothing_for_a_fresh_install() {
        let base = tempfile::tempdir().unwrap();
        assert!(migrate_dir(base.path().join("old"), base.path().join("new")).is_none());
    }

    #[test]
    fn reports_nothing_when_the_legacy_directory_is_empty() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("old");
        let current = base.path().join("new");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&current).unwrap();

        assert!(migrate_dir(legacy.clone(), current).is_none());
        // Tidied away, since it held nothing.
        assert!(!legacy.exists());
    }

    #[test]
    fn describe_names_both_paths() {
        let moved = Migration::Moved {
            from: PathBuf::from("/a/small-harness"),
            to: PathBuf::from("/a/albatross"),
        };
        assert_eq!(moved.describe(), "moved /a/small-harness → /a/albatross");

        let merged = Migration::Merged {
            from: PathBuf::from("/a/small-harness"),
            to: PathBuf::from("/a/albatross"),
            entries: 2,
            legacy_removed: false,
        };
        assert_eq!(
            merged.describe(),
            "carried 2 entries from /a/small-harness into /a/albatross \
             (kept the rest, already present at the new path)"
        );
    }
}
