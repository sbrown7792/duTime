use clap::Parser;

/// Replace the platform allocator.
///
/// The parallel walk allocates a path buffer per directory entry — 211k of
/// them on a 10 GiB `/usr`, across four threads — which is the shape of
/// workload a libc allocator's concurrency behaviour shows up in. Median of
/// five interleaved runs on that tree: glibc + system 0.78 s, glibc +
/// mimalloc 0.68 s, musl + system 1.65 s, musl + mimalloc 0.85 s. Owning the
/// allocator is what makes a static musl build shippable, and it stops
/// performance depending on which libc the binary was linked against.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
