use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// Chip YAML files used for register/field/event completions when the
    /// current .rseq buffer does not declare chip!(...).
    #[arg(long = "chip", value_name = "YAML")]
    chips: Vec<PathBuf>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    rseq_lsp::run_stdio(rseq_lsp::ServerOptions { chips: cli.chips }).await;
}
