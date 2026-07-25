use anyhow::Result;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestDiscovery {
    pub framework: String,
    pub test_files: Vec<String>,
    pub run_command: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
    pub exit_code: i32,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTestResult {
    pub framework: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub exit_code: i32,
    pub failures: Vec<String>,
    pub output_excerpt: String,
}

pub fn test_result_to_agent_json(framework: &str, result: &TestResult) -> AgentTestResult {
    const MAX_EXCERPT: usize = 2000;
    let output_excerpt = if result.output.chars().count() <= MAX_EXCERPT {
        result.output.clone()
    } else {
        let mut out: String = result.output.chars().take(MAX_EXCERPT).collect();
        out.push_str("…[truncated]");
        out
    };
    AgentTestResult {
        framework: framework.to_string(),
        total: result.total,
        passed: result.passed,
        failed: result.failed,
        skipped: result.skipped,
        exit_code: result.exit_code,
        failures: result.failures.clone(),
        output_excerpt,
    }
}

pub fn format_test_failure_feedback(result: &TestResult) -> String {
    let failures = if result.failures.is_empty() {
        result.output.chars().take(1500).collect::<String>()
    } else {
        result.failures.join("\n")
    };
    format!(
        "[Auto-verify after edits] {} test(s) failed ({} passed).\nFailures:\n{failures}",
        result.failed, result.passed
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestInvocation {
    pub program: String,
    pub args: Vec<String>,
}

pub fn discover_tests(workspace_root: &str) -> Result<TestDiscovery> {
    let root = Path::new(workspace_root);

    let python_tests = find_python_test_files(root)?;
    if root.join("pytest.ini").exists()
        || root.join("pyproject.toml").exists()
        || root.join("setup.py").exists()
        || root.join("requirements.txt").exists()
    {
        return Ok(TestDiscovery {
            framework: "pytest".to_string(),
            test_files: python_tests,
            run_command: Some("pytest".to_string()),
        });
    }

    if root.join("Cargo.toml").exists() {
        let test_files = find_rust_test_files(root)?;
        return Ok(TestDiscovery {
            framework: "cargo".to_string(),
            test_files,
            run_command: Some("cargo test".to_string()),
        });
    }

    if root.join("package.json").exists() {
        let test_files = find_js_test_files(root)?;
        let package_json = root.join("package.json");
        if let Ok(content) = fs::read_to_string(&package_json) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
                    if scripts.contains_key("test") {
                        return Ok(TestDiscovery {
                            framework: "npm".to_string(),
                            test_files,
                            run_command: Some("npm test".to_string()),
                        });
                    }
                }
            }
        }
    }

    if root.join("go.mod").exists() {
        let test_files = find_go_test_files(root)?;
        return Ok(TestDiscovery {
            framework: "go".to_string(),
            test_files,
            run_command: Some("go test ./...".to_string()),
        });
    }

    if !python_tests.is_empty() {
        return Ok(TestDiscovery {
            framework: "pytest".to_string(),
            test_files: python_tests,
            run_command: Some("pytest".to_string()),
        });
    }

    Ok(TestDiscovery {
        framework: "unknown".to_string(),
        test_files: vec![],
        run_command: None,
    })
}

fn find_python_test_files(root: &Path) -> Result<Vec<String>> {
    let mut test_files = Vec::new();

    for entry in workspace_files(root) {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "py") {
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if filename.starts_with("test_") || filename.ends_with("_test.py") {
                test_files.push(relative_slash_path(root, path));
            }
        }
    }

    Ok(test_files)
}

fn find_rust_test_files(root: &Path) -> Result<Vec<String>> {
    let mut test_files = Vec::new();

    for entry in workspace_files(root) {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let rel_path = relative_slash_path(root, path);
            if rel_path.starts_with("tests/") {
                test_files.push(rel_path);
                continue;
            }
            if fs::read_to_string(path)
                .map(|content| content.contains("#[test]") || content.contains("#[cfg(test)]"))
                .unwrap_or(false)
            {
                test_files.push(rel_path);
            }
        }
    }

    Ok(test_files)
}

fn find_js_test_files(root: &Path) -> Result<Vec<String>> {
    let mut test_files = Vec::new();

    for entry in workspace_files(root) {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| matches!(ext.to_str(), Some("js" | "ts" | "jsx" | "tsx")))
        {
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let rel_path = relative_slash_path(root, path);
            if filename.contains(".test.")
                || filename.contains(".spec.")
                || rel_path.contains("/__tests__/")
                || rel_path.starts_with("__tests__/")
            {
                test_files.push(rel_path);
            }
        }
    }

    Ok(test_files)
}

fn find_go_test_files(root: &Path) -> Result<Vec<String>> {
    let mut test_files = Vec::new();

    for entry in workspace_files(root) {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "go") {
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if filename.ends_with("_test.go") {
                test_files.push(relative_slash_path(root, path));
            }
        }
    }

    Ok(test_files)
}

pub fn run_tests(workspace_root: &str, pattern: Option<&str>) -> Result<TestResult> {
    let root = Path::new(workspace_root);
    let discovery = discover_tests(workspace_root)?;

    if discovery.framework == "unknown" {
        return Ok(TestResult {
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            failures: vec!["No test framework detected".to_string()],
            exit_code: 1,
            output: String::new(),
        });
    }

    let invocation = build_test_invocation(&discovery.framework, pattern);
    let output = run_invocation(root, &invocation)?;
    Ok(parse_completed_output(output, &discovery.framework))
}

pub fn run_selected_tests(workspace_root: &str, selected_tests: &[String]) -> Result<TestResult> {
    let root = Path::new(workspace_root);
    let discovery = discover_tests(workspace_root)?;
    if discovery.framework == "unknown" {
        return run_tests(workspace_root, None);
    }
    if selected_tests.is_empty() {
        return Ok(TestResult::default());
    }
    let invocation = build_selected_test_invocation(&discovery.framework, selected_tests);
    let output = run_invocation(root, &invocation)?;
    Ok(parse_completed_output(output, &discovery.framework))
}

pub fn build_test_invocation(framework: &str, pattern: Option<&str>) -> TestInvocation {
    let pattern = pattern.map(str::trim).filter(|p| !p.is_empty());
    match framework {
        "pytest" => TestInvocation {
            program: "pytest".into(),
            args: pattern
                .map(|p| vec!["-k".into(), p.into()])
                .unwrap_or_default(),
        },
        "cargo" => TestInvocation {
            program: "cargo".into(),
            args: {
                let mut args = vec!["test".into()];
                if let Some(pattern) = pattern {
                    args.push(pattern.into());
                }
                args
            },
        },
        "npm" => TestInvocation {
            program: "npm".into(),
            args: {
                let mut args = vec!["test".into()];
                if let Some(pattern) = pattern {
                    args.push("--".into());
                    args.push(pattern.into());
                }
                args
            },
        },
        "go" => TestInvocation {
            program: "go".into(),
            args: {
                let mut args = vec!["test".into(), "./...".into()];
                if let Some(pattern) = pattern {
                    args.push("-run".into());
                    args.push(pattern.into());
                }
                args
            },
        },
        _ => TestInvocation {
            program: String::new(),
            args: Vec::new(),
        },
    }
}

fn build_selected_test_invocation(framework: &str, selected_tests: &[String]) -> TestInvocation {
    match framework {
        "pytest" => TestInvocation {
            program: "pytest".into(),
            args: selected_tests.to_vec(),
        },
        "cargo" => TestInvocation {
            program: "cargo".into(),
            args: vec!["test".into()],
        },
        "npm" => TestInvocation {
            program: "npm".into(),
            args: {
                let mut args = vec!["test".into(), "--".into()];
                args.extend(selected_tests.iter().cloned());
                args
            },
        },
        "go" => TestInvocation {
            program: "go".into(),
            args: {
                let mut dirs = BTreeSet::new();
                for file in selected_tests {
                    let path = Path::new(file);
                    let dir = path.parent().unwrap_or_else(|| Path::new("."));
                    let dir = if dir.as_os_str().is_empty() {
                        ".".to_string()
                    } else {
                        format!("./{}", dir.display())
                    };
                    dirs.insert(dir);
                }
                let mut args = vec!["test".into()];
                args.extend(dirs);
                args
            },
        },
        _ => build_test_invocation(framework, None),
    }
}

fn run_invocation(root: &Path, invocation: &TestInvocation) -> Result<std::process::Output> {
    Ok(Command::new(&invocation.program)
        .args(&invocation.args)
        .current_dir(root)
        .output()?)
}

fn parse_completed_output(output: std::process::Output, framework: &str) -> TestResult {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined_output = format!("{}\n{}", stdout, stderr);

    let result = parse_test_results(&combined_output, framework);

    TestResult {
        output: combined_output,
        exit_code: output.status.code().unwrap_or(1),
        ..result
    }
}

pub fn parse_test_results(output: &str, framework: &str) -> TestResult {
    match framework {
        "pytest" => parse_pytest_output(output),
        "cargo" => parse_cargo_output(output),
        "npm" => parse_npm_output(output),
        "go" => parse_go_output(output),
        _ => TestResult {
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            failures: vec![],
            exit_code: 1,
            output: String::new(),
        },
    }
}

fn parse_pytest_output(output: &str) -> TestResult {
    let mut result = TestResult::default();

    for line in output.lines() {
        if line.contains(" passed") {
            if let Some(passed) = extract_number(line, "passed") {
                result.passed = passed;
            }
        }
        if line.contains(" failed") {
            if let Some(failed) = extract_number(line, "failed") {
                result.failed = failed;
            }
        }
        if line.contains(" skipped") {
            if let Some(skipped) = extract_number(line, "skipped") {
                result.skipped = skipped;
            }
        }
    }

    result.total = result.passed + result.failed + result.skipped;

    let mut failures = Vec::new();
    let mut in_failure = false;
    let mut current_failure = String::new();

    for line in output.lines() {
        if line.contains("FAILED") {
            in_failure = true;
            if !current_failure.is_empty() {
                failures.push(current_failure.clone());
                current_failure.clear();
            }
            current_failure.push_str(line);
        } else if in_failure {
            if line.is_empty() || line.starts_with("=") {
                in_failure = false;
                if !current_failure.is_empty() {
                    failures.push(current_failure.clone());
                    current_failure.clear();
                }
            } else {
                current_failure.push('\n');
                current_failure.push_str(line);
            }
        }
    }

    if !current_failure.is_empty() {
        failures.push(current_failure);
    }

    result.failures = failures;
    result
}

fn parse_cargo_output(output: &str) -> TestResult {
    let mut result = TestResult::default();

    for line in output.lines() {
        if line.contains("test result:") {
            if let Some(passed) = extract_number(line, "passed") {
                result.passed += passed;
            }
            if let Some(failed) = extract_number(line, "failed") {
                result.failed += failed;
            }
            if let Some(skipped) = extract_number(line, "ignored") {
                result.skipped += skipped;
            }
        }
    }

    result.total = result.passed + result.failed + result.skipped;

    let mut failures = Vec::new();
    let mut in_failure = false;
    let mut current_failure = String::new();

    for line in output.lines() {
        if line.contains("FAILED") {
            in_failure = true;
            if !current_failure.is_empty() {
                failures.push(current_failure.clone());
                current_failure.clear();
            }
            current_failure.push_str(line);
        } else if in_failure {
            if line.starts_with("test result:") || line.is_empty() {
                in_failure = false;
                if !current_failure.is_empty() {
                    failures.push(current_failure.clone());
                    current_failure.clear();
                }
            } else {
                current_failure.push('\n');
                current_failure.push_str(line);
            }
        }
    }

    if !current_failure.is_empty() {
        failures.push(current_failure);
    }

    result.failures = failures;
    result
}

fn parse_npm_output(output: &str) -> TestResult {
    let mut result = TestResult::default();

    for line in output.lines() {
        if line.contains("passing") || line.contains("✓") {
            result.passed += 1;
        }
        if line.contains("failing") || line.contains("✗") {
            result.failed += 1;
        }
    }

    result.total = result.passed + result.failed + result.skipped;
    result
}

fn parse_go_output(output: &str) -> TestResult {
    let mut result = TestResult::default();

    for line in output.lines() {
        if line.trim_start().starts_with("--- PASS:") {
            result.passed += 1;
        }
        if line.trim_start().starts_with("--- FAIL:") {
            result.failed += 1;
        }
    }

    result.total = result.passed + result.failed + result.skipped;
    result
}

fn extract_number(line: &str, keyword: &str) -> Option<usize> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == keyword || part.contains(keyword) {
            if i > 0 {
                if let Ok(num) = parts[i - 1].parse::<usize>() {
                    return Some(num);
                }
            }
            if i + 1 < parts.len() {
                if let Ok(num) = parts[i + 1].parse::<usize>() {
                    return Some(num);
                }
            }
        }
    }
    None
}

pub fn smart_test_selection(workspace_root: &str) -> Result<Vec<String>> {
    let root = Path::new(workspace_root);

    let diff_output = Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(root)
        .output()?;

    if !diff_output.status.success() {
        let discovery = discover_tests(workspace_root)?;
        return Ok(discovery.test_files);
    }

    let mut changed_files: Vec<String> = String::from_utf8_lossy(&diff_output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|s| s.to_string())
        .collect();
    let untracked_output = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(root)
        .output()?;
    if untracked_output.status.success() {
        for line in String::from_utf8_lossy(&untracked_output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
        {
            if !changed_files.iter().any(|path| path == line) {
                changed_files.push(line.to_string());
            }
        }
    }

    if changed_files.is_empty() {
        return Ok(vec![]);
    }

    let discovery = discover_tests(workspace_root)?;
    let mut selected_tests = Vec::new();

    for changed_file in &changed_files {
        let changed_path = Path::new(changed_file);
        let changed_stem = changed_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("");
        let changed_parent = changed_path
            .parent()
            .map(relative_dir_string)
            .unwrap_or_default();
        for test_file in &discovery.test_files {
            let test_path = Path::new(test_file);
            let test_parent = test_path
                .parent()
                .map(relative_dir_string)
                .unwrap_or_default();
            let test_stem = test_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("");
            let stem_match = !changed_stem.is_empty() && test_stem.contains(changed_stem);
            let directory_match =
                !changed_parent.is_empty() && test_parent.starts_with(&changed_parent);

            if (stem_match || directory_match) && !selected_tests.contains(test_file) {
                selected_tests.push(test_file.clone());
            }
        }
    }

    if selected_tests.is_empty() && !discovery.test_files.is_empty() {
        return Ok(discovery.test_files);
    }

    Ok(selected_tests)
}

fn workspace_files(root: &Path) -> Vec<ignore::DirEntry> {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(false)
        .require_git(false);
    builder.filter_entry(|entry| !has_skipped_component(entry.path()));
    builder
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|ft| ft.is_file()).unwrap_or(false))
        .collect()
}

fn has_skipped_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".git" | ".sessions" | "target" | "node_modules")
        )
    })
}

fn relative_slash_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn relative_dir_string(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        String::new()
    } else {
        path.components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_framework_specific_commands_without_shell() {
        let pytest = build_test_invocation("pytest", Some("test_name; rm -rf /"));
        assert_eq!(pytest.program, "pytest");
        assert_eq!(pytest.args, vec!["-k", "test_name; rm -rf /"]);

        let cargo = build_test_invocation("cargo", Some("my_test"));
        assert_eq!(cargo.program, "cargo");
        assert_eq!(cargo.args, vec!["test", "my_test"]);

        let go = build_test_invocation("go", Some("TestThing"));
        assert_eq!(go.program, "go");
        assert_eq!(go.args, vec!["test", "./...", "-run", "TestThing"]);
    }

    #[test]
    fn discovers_rust_tests_and_skips_target() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "#[cfg(test)]\nmod tests { #[test] fn works() {} }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("target/debug/generated.rs"),
            "#[test] fn nope() {}\n",
        )
        .unwrap();

        let discovery = discover_tests(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(discovery.framework, "cargo");
        assert_eq!(discovery.test_files, vec!["src/lib.rs"]);
    }

    #[test]
    fn cargo_parser_sums_multiple_result_lines() {
        let output = "\
test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";

        let result = parse_test_results(output, "cargo");

        assert_eq!(result.passed, 5);
        assert_eq!(result.skipped, 1);
        assert_eq!(result.total, 6);
    }

    #[test]
    fn smart_selection_includes_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn existing() {}\n").unwrap();
        fs::write(
            dir.path().join("tests/new_feature.rs"),
            "#[test] fn new_feature_works() {}\n",
        )
        .unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["add", "Cargo.toml", "src/lib.rs"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args([
                "-c",
                "user.name=Albatross",
                "-c",
                "user.email=albatross@example.invalid",
                "commit",
                "-m",
                "baseline",
            ])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let selected = smart_test_selection(dir.path().to_str().unwrap()).unwrap();

        assert!(selected.contains(&"tests/new_feature.rs".to_string()));
    }
}
