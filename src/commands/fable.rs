use anyhow::Result;

use crate::app_state::AppState;

const RESET: crate::theme::Style = crate::theme::RESET;
const DIM: crate::theme::Style = crate::theme::MUTED;
const YELLOW: crate::theme::Style = crate::theme::WARN;

pub(super) fn cmd_fable(args: &str, state: &AppState) -> Result<()> {
    let mut parts = args.split_whitespace();
    let action = parts.next().unwrap_or("show");

    match action {
        "" | "show" | "usage" | "current" => {
            if !state.config.fable.enabled {
                println!("  {DIM}Fable tracker disabled in config (fable.enabled = false){RESET}");
                return Ok(());
            }
            let report = crate::fable_usage::load_report(&state.config.fable)?;
            print!(
                "{}",
                crate::fable_usage::render_report(&report, state.config.scorecard.enabled)
            );
        }
        "path" => match crate::scorecard::scorecard_path() {
            Some(path) => println!("  {DIM}fableStore{RESET} {}", path.display()),
            None => println!(
                "  {YELLOW}!{RESET} {DIM}Fable tracker store unavailable: HOME is unset{RESET}"
            ),
        },
        "help" => print_fable_usage(),
        _ => print_fable_usage(),
    }

    Ok(())
}

fn print_fable_usage() {
    println!("  {DIM}Usage: /fable [show|usage|current|path|help]{RESET}");
    println!(
        "  {DIM}Tracks Albatross turns whose model id matches fable.fableModelMatches (default: `fable`).{RESET}"
    );
}
