use clap::Parser;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dutime=info".into()),
        )
        .with_target(false)
        .init();

    if let Err(e) = dutime::cli::run(dutime::cli::Cli::parse()) {
        eprintln!("dutime: {e:#}");
        std::process::exit(1);
    }
}
