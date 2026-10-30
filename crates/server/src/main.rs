use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--still-worker") {
        return match flicks_server::stills::worker() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("still: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    let cli = flicks_server::cli::Cli::parse();
    match flicks_server::cli::run(cli.command).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("flicks: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
