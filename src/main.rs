use clap::{Parser, Subcommand};
use dutime::scan::walker::{ScanOptions, scan};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "dutime", version, about = "Track disk usage over time")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan a directory once and print its totals.
    Scan {
        path: PathBuf,
        #[arg(long, default_value_t = 1 << 20)]
        min_file: i64,
        #[arg(long)]
        threads: Option<usize>,
        #[arg(long)]
        exclude: Vec<String>,
        /// Walk into other filesystems too.
        #[arg(long)]
        cross_filesystem: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dutime=info".into()),
        )
        .init();

    match Cli::parse().cmd {
        Cmd::Scan { path, min_file, threads, exclude, cross_filesystem } => {
            let mut opts = ScanOptions::new(&path);
            opts.track_file_min_bytes = min_file;
            opts.exclude = exclude;
            opts.one_filesystem = !cross_filesystem;
            if let Some(t) = threads {
                opts.threads = t;
            }

            let t0 = std::time::Instant::now();
            let r = scan(&opts)?;
            let elapsed = t0.elapsed();
            let roll = r.tree.rollup();

            println!("path              {}", path.display());
            println!("apparent bytes    {}", roll.bytes[0]);
            println!("allocated bytes   {}", roll.blocks[0]);
            println!("dirs              {}", r.stats.n_dirs);
            println!("files             {}", r.stats.n_files);
            println!("tracked entities  {}", r.tree.len());
            println!("hardlinks deduped {}", r.stats.n_hardlinks_deduped);
            println!("errors            {}", r.stats.n_errors);
            println!("skipped mounts    {}", r.stats.skipped_mounts.len());
            println!("threads           {}", opts.threads);
            println!("elapsed           {:.3}s", elapsed.as_secs_f64());
        }
    }
    Ok(())
}
