use clap::Parser;
use nudge::cli::Cli;

fn main() {
    let cli = Cli::parse();
    if let Err(e) = nudge::run(cli) {
        eprintln!("nudge: {e}");
        std::process::exit(1);
    }
}
