use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = flicks_server::cli::Cli::parse();
    match flicks_server::cli::run(cli.command).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("flicks: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
