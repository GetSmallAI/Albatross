#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
    pub hunks: usize,
}

pub fn diff_stats(diff: &str) -> DiffStats {
    let mut stats = DiffStats {
        added: 0,
        removed: 0,
        hunks: 0,
    };
    for line in diff.lines() {
        if line.starts_with("@@") {
            stats.hunks += 1;
        } else if line.starts_with('+') && !line.starts_with("+++ ") {
            stats.added += 1;
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            stats.removed += 1;
        }
    }
    stats
}

pub fn unified_diff(old_text: &str, new_text: &str, path: &str) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut out: Vec<String> = vec![format!("--- {path}"), format!("+++ {path}")];
    let (mut i, mut j) = (0usize, 0usize);
    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() && old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
            continue;
        }
        out.push(format!("@@ -{} +{} @@", i + 1, j + 1));
        while i < old_lines.len() && (j >= new_lines.len() || old_lines[i] != new_lines[j]) {
            out.push(format!("-{}", old_lines[i]));
            i += 1;
        }
        while j < new_lines.len() && (i >= old_lines.len() || old_lines[i] != new_lines[j]) {
            out.push(format!("+{}", new_lines[j]));
            j += 1;
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_single_replacement() {
        let d = unified_diff("a\nb\nc", "a\nB\nc", "f.txt");
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
        assert!(d.contains("@@ -2 +2 @@"));
    }

    #[test]
    fn stats_ignore_headers_but_count_content_that_starts_with_diff_markers() {
        let diff = "--- f.txt\n+++ f.txt\n@@ -1 +1 @@\n---old\n+++new";

        assert_eq!(
            diff_stats(diff),
            DiffStats {
                added: 1,
                removed: 1,
                hunks: 1,
            }
        );
    }

    #[test]
    fn newline_terminated_replacement_counts_only_changed_content() {
        let diff = unified_diff("alpha\nbeta\n", "alpha\nbeta-polished\n", "f.txt");

        assert_eq!(
            diff_stats(&diff),
            DiffStats {
                added: 1,
                removed: 1,
                hunks: 1,
            }
        );
    }
}
