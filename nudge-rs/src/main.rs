use std::io::IsTerminal;

use clap::Parser;
use nudge::app::{hold_popup_open, route, should_hold};
use nudge::cli::Cli;
use nudge::here::Here;

fn main() {
    let cli = Cli::parse();
    // Decided here, around `run`, so that the wait comes after whatever nudge
    // prints, an error included: an error is what a popup most needs to keep
    // on screen.
    let here = Here::from_env();
    let route = route(&cli, std::io::stdout().is_terminal());
    let result = nudge::run(&cli);
    if let Err(e) = &result {
        eprintln!("nudge: {e}");
    }
    if should_hold(&route, &here, std::io::stdin().is_terminal()) {
        hold_popup_open();
    }
    if result.is_err() {
        std::process::exit(1);
    }
}
