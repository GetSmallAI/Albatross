//! Workflow command group: eval, play, fix, batch/refactor, test, and
//! prompt-library commands. Split out of commands.rs; dispatch lives in
//! mod.rs.

use super::context_cmds::last_user_prompt;
use super::*;

fn parse_eval_model(spec: &str, state: &AppState) -> (BackendDescriptor, String) {
    if let Some((prefix, model)) = spec.split_once(':') {
        if let Some(name) = BackendName::parse(prefix) {
            return (backend(name), model.to_string());
        }
    }
    (state.backend.clone(), spec.to_string())
}

async fn eval_once(
    state: &AppState,
    backend_desc: &BackendDescriptor,
    model: &str,
    prompt: &str,
    tools_on: bool,
) -> Result<String> {
    let messages = vec![ChatMessage::User {
        content: prompt.to_string().into(),
    }];
    let tool_defs = if tools_on {
        let tools = build_tools(&state.config);
        to_openai_tools(&tools)
    } else {
        Vec::new()
    };
    let req = ChatRequest {
        model,
        messages: &messages,
        tools: if tool_defs.is_empty() {
            None
        } else {
            Some(&tool_defs)
        },
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: false,
        }),
        max_tokens: None,
        prompt_cache_key: None,
        effort: None,
    };
    let mut out = String::new();
    stream_chat(&state.http, backend_desc, &req, None, |chunk| {
        if let Some(choice) = chunk.choices.first() {
            if let Some(content) = &choice.delta.content {
                out.push_str(content);
            }
        }
    })
    .await?;
    Ok(out)
}

pub(super) async fn cmd_play(args: &str, state: &mut AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if args.trim().is_empty() {
        print_play_list();
        return Ok(());
    }
    if parts.first() == Some(&"exit") {
        let fixture = state
            .play_session
            .as_ref()
            .map(|s| s.fixture_id.clone())
            .unwrap_or_else(|| "session".into());
        restore_play_session(state)?;
        println!("  {GREEN}✓{RESET} {DIM}play ended ({fixture}) — workspace restored{RESET}");
        return Ok(());
    }
    if parts.first() == Some(&"score") {
        if let Some(score) = state.last_play_scorecard.as_ref() {
            print_scorecard(state, score);
        } else {
            println!("  {DIM}No play scorecard yet.{RESET}");
        }
        return Ok(());
    }

    let yolo = parts.contains(&"--yolo");
    let filtered: Vec<&str> = parts.iter().copied().filter(|p| *p != "--yolo").collect();

    if filtered.first() == Some(&"battle") {
        let fixture = filtered
            .get(1)
            .ok_or_else(|| anyhow!("usage: /play battle <fixture> <model[,model]>"))?;
        let models = filtered
            .get(2)
            .map(|s| s.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec![state.model.clone()]);
        run_play_battle(state, fixture, &models).await?;
        return Ok(());
    }

    let fixture = filtered
        .first()
        .ok_or_else(|| anyhow!("usage: /play <fixture> [--yolo]"))?;
    run_play_fixture(state, fixture, yolo).await
}

pub(super) async fn cmd_fix(args: &str, state: &mut AppState) -> Result<()> {
    let opts = parse_fix_args(args, state.config.fix.max_attempts)?;
    run_fix_loop(state, opts).await
}

pub(super) async fn cmd_iterate(args: &str, state: &mut AppState) -> Result<()> {
    let opts = parse_iterate_args(
        args,
        state.config.iterate.max_iters,
        state.config.rubric.pass_threshold,
    )?;
    run_iterate_loop(state, opts).await
}

pub(super) async fn cmd_auto(args: &str, state: &mut AppState) -> Result<()> {
    let mut opts = parse_auto_args(
        args,
        state.config.auto.max_rounds,
        state.config.rubric.pass_threshold,
        state.config.auto.reset_ratio,
    )?;
    // CLI flags win; fall back to the AutoConfig defaults otherwise.
    if opts.budget_usd.is_none() {
        opts.budget_usd = state.config.auto.budget_usd;
    }
    if opts.deadline.is_none() {
        if let Some(d) = &state.config.auto.deadline {
            opts.deadline = Some(crate::auto_loop::parse_duration(d)?);
        }
    }
    run_auto_loop(state, opts).await
}

pub(super) async fn cmd_eval(args: &str, state: &AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.first() == Some(&"agent") {
        return cmd_eval_agent(&parts[1..], state).await;
    }
    let (prompts, model_specs) = if let Some(first) = parts.first() {
        if Path::new(first).exists() {
            let text = fs::read_to_string(first)?;
            let prompts: Vec<String> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string)
                .collect();
            let models = parts
                .get(1)
                .map(|s| s.split(',').map(str::to_string).collect())
                .unwrap_or_else(|| vec![state.model.clone()]);
            (prompts, models)
        } else {
            let prompts = last_user_prompt(state)
                .map(|p| vec![p])
                .ok_or_else(|| anyhow!("No prompt file found and no prior user message"))?;
            let models = first.split(',').map(str::to_string).collect();
            (prompts, models)
        }
    } else {
        let prompts = last_user_prompt(state)
            .map(|p| vec![p])
            .ok_or_else(|| anyhow!("No prior user message to evaluate"))?;
        (prompts, vec![state.model.clone()])
    };
    if prompts.is_empty() {
        println!("  {DIM}No prompts to evaluate.{RESET}");
        return Ok(());
    }
    let eval_dir = Path::new(&state.session_dir).join("evals");
    fs::create_dir_all(&eval_dir)?;
    let id = Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
    let mut json_results = Vec::new();
    let mut md = String::from("# Albatross Eval\n\n");
    for spec in model_specs {
        let (backend_desc, model) = parse_eval_model(&spec, state);
        validate(&backend_desc)?;
        for prompt in &prompts {
            for tools_on in [false, true] {
                println!(
                    "  {DIM}eval {} · tools={} · {}{RESET}",
                    backend_desc.name.as_str(),
                    tools_on,
                    model
                );
                let start = Instant::now();
                let result = eval_once(state, &backend_desc, &model, prompt, tools_on).await;
                let elapsed_ms = start.elapsed().as_millis();
                let output = result.unwrap_or_else(|e| format!("ERROR: {e}"));
                json_results.push(serde_json::json!({
                    "backend": backend_desc.name.as_str(),
                    "model": model.clone(),
                    "toolsOn": tools_on,
                    "prompt": prompt,
                    "output": output,
                    "elapsedMs": elapsed_ms,
                }));
                md.push_str(&format!(
                    "## {} · {} · tools={}\n\n**Prompt**\n\n{}\n\n**Output**\n\n{}\n\n",
                    backend_desc.name.as_str(),
                    model,
                    tools_on,
                    prompt,
                    json_results
                        .last()
                        .and_then(|v| v.get("output"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                ));
            }
        }
    }
    let json_path = eval_dir.join(format!("{id}.json"));
    let md_path = eval_dir.join(format!("{id}.md"));
    fs::write(&json_path, serde_json::to_string_pretty(&json_results)?)?;
    fs::write(&md_path, md)?;
    println!(
        "  {GREEN}✓{RESET} {DIM}eval saved → {} and {}{RESET}",
        json_path.display(),
        md_path.display()
    );
    Ok(())
}

async fn cmd_eval_agent(parts: &[&str], state: &AppState) -> Result<()> {
    let (fixture_ids, model_specs) = if parts.is_empty() {
        (
            vec!["fix-failing-test".to_string()],
            vec![state.model.clone()],
        )
    } else if parts[0] == "all" {
        let ids = builtin_fixtures()
            .into_iter()
            .map(|f| f.id)
            .collect::<Vec<_>>();
        let models = if parts.len() > 1 {
            parts[1].split(',').map(str::to_string).collect()
        } else {
            vec![state.model.clone()]
        };
        (ids, models)
    } else {
        let fixture = parts[0].to_string();
        let models = if parts.len() > 1 {
            parts[1].split(',').map(str::to_string).collect()
        } else {
            vec![state.model.clone()]
        };
        (vec![fixture], models)
    };
    let fixtures = resolve_eval_agent_fixtures(&fixture_ids)?;

    let eval_dir = Path::new(&state.session_dir).join("evals");
    fs::create_dir_all(&eval_dir)?;
    let id = Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
    let mut all_results = Vec::new();

    for spec in model_specs {
        let (backend_desc, model) = parse_eval_model(&spec, state);
        validate(&backend_desc)?;
        for fixture in &fixtures {
            println!(
                "  {DIM}agent eval {} · {} · {}{RESET}",
                fixture.id,
                backend_desc.name.as_str(),
                model
            );
            let result = run_agent_eval(&state.config, &backend_desc, &model, fixture).await?;
            println!(
                "  {} {}{RESET}",
                if result.passed {
                    format!("{GREEN}✓{RESET}")
                } else {
                    format!("{RED}✗{RESET}")
                },
                if result.passed { "passed" } else { "failed" }
            );
            all_results.push(result);
        }
    }

    let json_path = eval_dir.join(format!("agent-{id}.json"));
    let md_path = eval_dir.join(format!("agent-{id}.md"));
    fs::write(&json_path, serde_json::to_string_pretty(&all_results)?)?;
    fs::write(&md_path, render_agent_eval_markdown(&all_results))?;
    println!(
        "  {GREEN}✓{RESET} {DIM}agent eval saved → {} and {}{RESET}",
        json_path.display(),
        md_path.display()
    );
    Ok(())
}

fn resolve_eval_agent_fixtures(
    fixture_specs: &[String],
) -> Result<Vec<crate::agent_eval::AgentEvalFixture>> {
    fixture_specs
        .iter()
        .map(|spec| crate::agent_eval::fixture_by_spec(spec))
        .collect()
}

pub(super) fn cmd_batch(args: &str, state: &AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.is_empty() {
        println!("  {DIM}Usage: /batch [preview|apply] [operations.json]{RESET}");
        println!("  {DIM}  /batch preview ops.json  - Show what would change{RESET}");
        println!("  {DIM}  /batch apply ops.json    - Execute the operations{RESET}");
        return Ok(());
    }

    let action = parts[0];
    let json_path = if parts.len() > 1 {
        PathBuf::from(parts[1])
    } else {
        println!(
            "  {YELLOW}!{RESET} {DIM}Please provide a path to the operations JSON file.{RESET}"
        );
        return Ok(());
    };

    if !json_path.exists() {
        println!(
            "  {RED}✗{RESET} {DIM}Operations file not found: {}{RESET}",
            json_path.display()
        );
        return Ok(());
    }

    let json_content = fs::read_to_string(&json_path)?;
    let operations: Vec<BatchEditOperation> = match serde_json::from_str(&json_content) {
        Ok(ops) => ops,
        Err(e) => {
            println!("  {RED}✗{RESET} {DIM}Failed to parse operations: {e}{RESET}");
            return Ok(());
        }
    };

    match action {
        "preview" => {
            let preview = preview_batch_operations(&operations);
            println!("  {DIM}Batch Operations Preview{RESET}");
            println!("  {DIM}Total files: {}{RESET}", preview.total_files);
            println!(
                "  {DIM}Estimated changes: {}{RESET}",
                preview.estimated_changes
            );
            println!();
            for (i, op) in preview.operations.iter().enumerate() {
                println!("  {CYAN}{}. {}{RESET}", i + 1, op.file_path);
                match &op.operation {
                    EditOperation::Replace { old_string, .. } => {
                        println!(
                            "    {DIM}Replace: {}{RESET}",
                            truncate_string(old_string, 60)
                        );
                    }
                    EditOperation::Insert { position, .. } => {
                        println!("    {DIM}Insert: {:?}{RESET}", position);
                    }
                    EditOperation::Delete { pattern } => {
                        println!("    {DIM}Delete: {}{RESET}", truncate_string(pattern, 60));
                    }
                }
            }
            let workspace_root = Path::new(&state.config.workspace_root);
            let dry_run = execute_batch_operations(&operations, workspace_root, true)?;
            if dry_run.failed.is_empty() {
                println!(
                    "  {GREEN}✓{RESET} {DIM}batch is valid; {} file(s) would change{RESET}",
                    dry_run.skipped.len()
                );
            } else {
                println!(
                    "  {RED}✗{RESET} {DIM}batch has {} validation error(s){RESET}",
                    dry_run.failed.len()
                );
                for fail in &dry_run.failed {
                    println!("    {RED}{}: {}{RESET}", fail.file_path, fail.error);
                }
            }
        }
        "apply" => {
            println!("  {DIM}Applying batch operations...{RESET}");
            let workspace_root = Path::new(&state.config.workspace_root);
            let result = execute_batch_operations(&operations, workspace_root, false)?;

            if !result.successful.is_empty() {
                println!(
                    "  {GREEN}✓{RESET} {DIM}Successfully applied {} file(s){RESET}",
                    result.successful.len()
                );
                for file in &result.successful {
                    println!("    {DIM}{}{RESET}", file);
                }
            }

            if !result.failed.is_empty() {
                println!(
                    "  {RED}✗{RESET} {DIM}Failed on {} file(s){RESET}",
                    result.failed.len()
                );
                for fail in &result.failed {
                    println!("    {RED}{}: {}{RESET}", fail.file_path, fail.error);
                }
            }

            if !result.skipped.is_empty() {
                println!(
                    "  {YELLOW}!{RESET} {DIM}Skipped {} file(s){RESET}",
                    result.skipped.len()
                );
                for file in &result.skipped {
                    println!("    {DIM}{}{RESET}", file);
                }
            }
        }
        _ => {
            println!("  {DIM}Usage: /batch [preview|apply] [operations.json]{RESET}");
        }
    }

    Ok(())
}

pub(super) fn cmd_refactor(args: &str, state: &AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.is_empty() {
        println!("  {DIM}Usage: /refactor [references|related] <file_path>{RESET}");
        println!("  {DIM}  /refactor references src/main.rs  - Find cross-file references{RESET}");
        println!("  {DIM}  /refactor related src/main.rs     - Find related files{RESET}");
        return Ok(());
    }

    let action = parts[0];
    if parts.len() < 2 {
        println!("  {YELLOW}!{RESET} {DIM}Please provide a file path.{RESET}");
        return Ok(());
    }

    let file_path = parts[1];

    match action {
        "references" => {
            let references = find_cross_file_references(&state.config, file_path)?;
            if references.is_empty() {
                println!(
                    "  {DIM}No cross-file references found for {}{RESET}",
                    file_path
                );
            } else {
                println!("  {DIM}Cross-file references for {}{RESET}", file_path);
                for ref_info in references {
                    println!(
                        "    {CYAN}{}{RESET} {DIM}→{}{RESET}",
                        ref_info.from_file, ref_info.to_file
                    );
                    println!(
                        "      {DIM}type: {}, line: {}{RESET}",
                        ref_info.reference_type, ref_info.line
                    );
                }
            }
        }
        "related" => {
            let related = find_related_files(&state.config, file_path)?;
            if related.is_empty() {
                println!("  {DIM}No related files found for {}{RESET}", file_path);
            } else {
                println!("  {DIM}Related files for {}{RESET}", file_path);
                for file in related {
                    println!("    {CYAN}{}{RESET}", file);
                }
            }
        }
        _ => {
            println!("  {DIM}Usage: /refactor [references|related] <file_path>{RESET}");
        }
    }

    Ok(())
}

pub(super) fn cmd_test(args: &str, state: &AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.is_empty() {
        println!("  {DIM}Usage: /test [discover|run|smart] [args]{RESET}");
        println!("  {DIM}  /test discover              - Auto-detect test framework and list tests{RESET}");
        println!("  {DIM}  /test run [test_pattern]   - Run all tests or matching pattern{RESET}");
        println!("  {DIM}  /test smart                 - Run tests based on changed files{RESET}");
        return Ok(());
    }

    let action = parts[0];

    match action {
        "discover" => match discover_tests(&state.config.workspace_root) {
            Ok(discovery) => {
                println!("  {DIM}Test Discovery{RESET}");
                println!("  {DIM}Framework: {}{RESET}", discovery.framework);
                println!(
                    "  {DIM}Test files found: {}{RESET}",
                    discovery.test_files.len()
                );
                for test_file in &discovery.test_files {
                    println!("    {CYAN}{}{RESET}", test_file);
                }
                if let Some(command) = discovery.run_command {
                    println!("  {DIM}Run command: {}{RESET}", command);
                }
            }
            Err(e) => {
                println!("  {RED}✗{RESET} {DIM}Test discovery failed: {e}{RESET}");
            }
        },
        "run" => {
            let pattern = if parts.len() > 1 {
                Some(parts[1])
            } else {
                None
            };
            match run_tests(&state.config.workspace_root, pattern) {
                Ok(results) => {
                    println!("  {DIM}Test Results{RESET}");
                    println!("  {DIM}Total: {}{RESET}", results.total);
                    println!("  {DIM}Passed: {}{RESET}", results.passed);
                    println!("  {DIM}Failed: {}{RESET}", results.failed);
                    println!("  {DIM}Skipped: {}{RESET}", results.skipped);
                    if !results.failures.is_empty() {
                        println!();
                        println!("  {RED}Failures:{RESET}");
                        for failure in &results.failures {
                            println!("    {RED}{}{RESET}", failure);
                        }
                    }
                    if results.exit_code != 0 {
                        println!();
                        println!(
                            "  {RED}✗{RESET} {DIM}Tests failed with exit code {}{RESET}",
                            results.exit_code
                        );
                    } else {
                        println!();
                        println!("  {GREEN}✓{RESET} {DIM}All tests passed{RESET}");
                    }
                }
                Err(e) => {
                    println!("  {RED}✗{RESET} {DIM}Test execution failed: {e}{RESET}");
                }
            }
        }
        "smart" => match smart_test_selection(&state.config.workspace_root) {
            Ok(selected_tests) => {
                println!("  {DIM}Smart Test Selection{RESET}");
                println!(
                    "  {DIM}Based on changed files, {} test(s) selected{RESET}",
                    selected_tests.len()
                );
                for test in &selected_tests {
                    println!("    {CYAN}{}{RESET}", test);
                }
                if !selected_tests.is_empty() {
                    println!();
                    println!("  {DIM}Running selected tests...{RESET}");
                    match run_selected_tests(&state.config.workspace_root, &selected_tests) {
                        Ok(results) => {
                            println!(
                                "  {DIM}Total: {} Passed: {} Failed: {}{RESET}",
                                results.total, results.passed, results.failed
                            );
                        }
                        Err(e) => {
                            println!("  {RED}✗{RESET} {DIM}Test execution failed: {e}{RESET}");
                        }
                    }
                }
            }
            Err(e) => {
                println!("  {RED}✗{RESET} {DIM}Smart test selection failed: {e}{RESET}");
            }
        },
        _ => {
            println!("  {DIM}Usage: /test [discover|run|smart] [args]{RESET}");
        }
    }

    Ok(())
}

pub(super) async fn cmd_prompt(args: &str, state: &mut AppState) -> Result<()> {
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.is_empty() {
        println!("  {DIM}Usage: /prompt [save|list|run|template|builtin|delete|export|import] [args]{RESET}");
        println!(
            "  {DIM}  /prompt save <name> [text]    - Save text or the last user prompt{RESET}"
        );
        println!("  {DIM}  /prompt list                  - List saved prompts{RESET}");
        println!("  {DIM}  /prompt run <name> [k=v]      - Run a saved or built-in prompt{RESET}");
        println!("  {DIM}  /prompt template <name>       - Create parameterized template{RESET}");
        println!("  {DIM}  /prompt builtin                - List built-in prompts{RESET}");
        println!("  {DIM}  /prompt builtin <name>        - Show a built-in prompt{RESET}");
        println!("  {DIM}  /prompt delete <name>         - Delete a saved prompt{RESET}");
        println!("  {DIM}  /prompt export <path>         - Export all prompts to JSON{RESET}");
        println!("  {DIM}  /prompt import <path>         - Import prompts from JSON{RESET}");
        return Ok(());
    }

    let action = parts[0];
    let library = PromptLibrary::new();

    match action {
        "save" => {
            if parts.len() < 2 {
                println!("  {YELLOW}!{RESET} {DIM}Please provide a name for the prompt.{RESET}");
                return Ok(());
            }
            let name = parts[1];
            let content = args
                .splitn(3, ' ')
                .nth(2)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .or_else(|| latest_user_prompt(&state.messages));
            let Some(content) = content else {
                println!("  {YELLOW}!{RESET} {DIM}No prompt text provided and no prior user prompt found.{RESET}");
                return Ok(());
            };
            save_prompt(&state.config.session_dir, name, &content)?;
            println!("  {GREEN}✓{RESET} {DIM}Prompt '{}' saved{RESET}", name);
        }
        "list" => {
            let user_prompts = list_prompts(&state.config.session_dir)?;

            if user_prompts.is_empty() {
                println!("  {DIM}No saved prompts found.{RESET}");
            } else {
                println!("  {DIM}Saved prompts:{RESET}");
                for name in &user_prompts {
                    println!("    {CYAN}{}{RESET}", name);
                }
            }

            println!();
            println!("  {DIM}Built-in prompts:{RESET}");
            for template in library.list() {
                println!(
                    "    {CYAN}{}{RESET} - {}",
                    template.name, template.description
                );
            }
        }
        "run" => {
            if parts.len() < 2 {
                println!("  {YELLOW}!{RESET} {DIM}Please provide a prompt name.{RESET}");
                return Ok(());
            }
            let name = parts[1];

            if name.starts_with("builtin:") {
                let builtin_name = name.strip_prefix("builtin:").unwrap();
                if let Some(template) = library.get(builtin_name) {
                    let variables = parse_prompt_variables(&parts[2..]);
                    let missing: Vec<&str> = template
                        .variables
                        .iter()
                        .map(String::as_str)
                        .filter(|name| !variables.contains_key(*name))
                        .collect();
                    if !missing.is_empty() {
                        println!(
                            "  {YELLOW}!{RESET} {DIM}Missing variable(s): {}{RESET}",
                            missing.join(", ")
                        );
                        println!(
                            "  {DIM}Pass variables as key=value after the prompt name.{RESET}"
                        );
                        return Ok(());
                    }
                    let content = library.render(builtin_name, &variables)?;
                    run_prompt_content(&content, state).await?;
                } else {
                    println!(
                        "  {RED}✗{RESET} {DIM}Built-in prompt not found: {}{RESET}",
                        builtin_name
                    );
                }
            } else {
                match load_prompt(&state.config.session_dir, name) {
                    Ok(content) => {
                        run_prompt_content(&content, state).await?;
                    }
                    Err(_) => {
                        println!("  {RED}✗{RESET} {DIM}Prompt not found: {}{RESET}", name);
                    }
                }
            }
        }
        "template" => {
            if parts.len() < 2 {
                println!("  {YELLOW}!{RESET} {DIM}Please provide a template name.{RESET}");
                return Ok(());
            }
            let name = parts[1];
            let template_content = r#"# Parameterized Template: {{name}}
# Variables: {{var1}}, {{var2}}
# Use {{variable}} syntax for parameters

Your prompt template goes here.
Use double curly braces {{}} for variable placeholders."#;
            save_prompt(&state.config.session_dir, name, template_content)?;
            println!("  {GREEN}✓{RESET} {DIM}Template '{}' saved{RESET}", name);
        }
        "builtin" => {
            if parts.len() < 2 {
                println!("  {DIM}Built-in prompts:{RESET}");
                for template in library.list() {
                    println!(
                        "    {CYAN}{}{RESET} - {}",
                        template.name, template.description
                    );
                }
                println!();
                println!("  {DIM}Use: /prompt run builtin:<name>{RESET}");
            } else {
                let name = parts[1];
                if let Some(template) = library.get(name) {
                    println!("  {DIM}Built-in prompt: {}{RESET}", template.name);
                    println!("  {DIM}Description: {}{RESET}", template.description);
                    println!();
                    println!("{}", template.content);
                    if !template.variables.is_empty() {
                        println!();
                        println!("  {DIM}Variables: {}{RESET}", template.variables.join(", "));
                    }
                } else {
                    println!(
                        "  {RED}✗{RESET} {DIM}Built-in prompt not found: {}{RESET}",
                        name
                    );
                }
            }
        }
        "delete" => {
            if parts.len() < 2 {
                println!("  {YELLOW}!{RESET} {DIM}Please provide a prompt name.{RESET}");
                return Ok(());
            }
            let name = parts[1];
            match delete_prompt(&state.config.session_dir, name) {
                Ok(_) => println!("  {GREEN}✓{RESET} {DIM}Prompt '{}' deleted{RESET}", name),
                Err(e) => println!("  {RED}✗{RESET} {DIM}{}{RESET}", e),
            }
        }
        "export" => {
            if parts.len() < 2 {
                println!("  {YELLOW}!{RESET} {DIM}Please provide an export path.{RESET}");
                return Ok(());
            }
            let export_path = PathBuf::from(parts[1]);
            match export_prompts(&state.config.session_dir, &export_path) {
                Ok(_) => println!(
                    "  {GREEN}✓{RESET} {DIM}Prompts exported to {}{RESET}",
                    export_path.display()
                ),
                Err(e) => println!("  {RED}✗{RESET} {DIM}{}{RESET}", e),
            }
        }
        "import" => {
            if parts.len() < 2 {
                println!("  {YELLOW}!{RESET} {DIM}Please provide an import path.{RESET}");
                return Ok(());
            }
            let import_path = PathBuf::from(parts[1]);
            match import_prompts(&state.config.session_dir, &import_path) {
                Ok(count) => println!("  {GREEN}✓{RESET} {DIM}Imported {} prompt(s){RESET}", count),
                Err(e) => println!("  {RED}✗{RESET} {DIM}{}{RESET}", e),
            }
        }
        _ => {
            println!("  {DIM}Usage: /prompt [save|list|run|template|builtin|delete|export|import] [args]{RESET}");
        }
    }

    Ok(())
}

fn latest_user_prompt(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find_map(|m| m.user_text().map(|s| s.into_owned()))
}

fn parse_prompt_variables(parts: &[&str]) -> HashMap<String, String> {
    parts
        .iter()
        .filter_map(|part| part.split_once('='))
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

async fn run_prompt_content(content: &str, state: &mut AppState) -> Result<()> {
    let needs_system = !matches!(state.messages.first(), Some(ChatMessage::System { .. }));
    if needs_system {
        let system = ChatMessage::System {
            content: state.config.render_system_prompt_for_tools(&[]),
        };
        save_message(&state.session_path, &system)?;
        state.messages.insert(0, system);
    }

    let user_msg = ChatMessage::User {
        content: content.to_string().into(),
    };
    state.messages.push(user_msg.clone());
    save_message(&state.session_path, &user_msg)?;

    let request_messages = state.messages.clone();
    let req = ChatRequest {
        model: &state.model,
        messages: &request_messages,
        tools: None,
        stream: true,
        stream_options: Some(StreamOptions {
            include_usage: true,
        }),
        max_tokens: None,
        prompt_cache_key: None,
        effort: state.active_effort,
    };

    println!(
        "  {DIM}running prompt with {} · {}{RESET}",
        state.config.backend.as_str(),
        state.model
    );
    let mut assistant = String::new();
    let mut input_tokens = 0;
    let mut output_tokens = 0;
    let mut out = std::io::stdout();
    stream_chat(&state.http, &state.backend, &req, None, |chunk| {
        if let Some(usage) = chunk.usage {
            input_tokens = usage.prompt_tokens;
            output_tokens = usage.completion_tokens;
        }
        if let Some(choice) = chunk.choices.first() {
            if let Some(delta) = &choice.delta.content {
                assistant.push_str(delta);
                let _ = out.write_all(delta.as_bytes());
                let _ = out.flush();
            }
        }
    })
    .await?;
    println!();

    let assistant_msg = ChatMessage::Assistant {
        content: Some(assistant),
        tool_calls: Vec::new(),
        provider_content: Vec::new(),
    };
    state.messages.push(assistant_msg.clone());
    save_message(&state.session_path, &assistant_msg)?;
    state.total_in += input_tokens;
    state.total_out += output_tokens;
    Ok(())
}

fn truncate_string(s: &str, max_len: usize) -> String {
    // Cap by Unicode scalars so multi-byte edit text cannot panic on a mid-char slice.
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    let keep = max_len.saturating_sub(3);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_string_keeps_ascii_short_values() {
        assert_eq!(truncate_string("hello", 60), "hello");
        assert_eq!(
            truncate_string(
                "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
                60
            ),
            "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTU..."
        );
    }

    #[test]
    fn truncate_string_does_not_panic_on_multibyte_over_byte_cap() {
        let s = "á".repeat(40);
        assert_eq!(truncate_string(&s, 60), s);
    }

    #[test]
    fn truncate_string_caps_by_unicode_scalars() {
        let s = "á".repeat(70);
        let out = truncate_string(&s, 60);
        assert_eq!(out, format!("{}...", "á".repeat(57)));
    }

    #[test]
    fn eval_agent_resolves_external_fixture_specs() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("workspace/basic/src")).unwrap();
        fs::write(
            dir.path().join("workspace/basic/Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("workspace/basic/src/lib.rs"),
            "pub fn add() {}\n",
        )
        .unwrap();
        let fixture_path = dir.path().join("external.json");
        fs::write(
            &fixture_path,
            r#"{
              "id": "external-basic",
              "prompt": "Read the library.",
              "workspace": "workspace/basic",
              "checks": [
                { "type": "fileContains", "path": "src/lib.rs", "needle": "add" }
              ]
            }"#,
        )
        .unwrap();
        let spec = fixture_path.to_str().unwrap();

        let fixtures = resolve_eval_agent_fixtures(&[spec.to_string()]).unwrap();

        assert_eq!(fixtures.len(), 1);
        assert_eq!(fixtures[0].id, "external-basic");
        assert!(fixtures[0].fixture_root.is_some());
    }
}
