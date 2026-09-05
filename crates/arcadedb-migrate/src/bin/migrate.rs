use arcadedb_migrate::cli;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match cli::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // The typed Display impls inline their sources (context: cause),
            // so the plain format carries the full story.
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
