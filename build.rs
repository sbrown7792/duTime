//! Make asset edits trigger a rebuild.
//!
//! `rust-embed` bakes `web/` into the binary at compile time, but Cargo has no
//! idea those files are inputs — so editing the UI and rebuilding silently
//! ships the *previous* assets. That failure is near-invisible: the binary
//! builds, the server starts, and you debug a change that was never deployed.
//!
//! The same reasoning drives the build stamp below. duTime is deployed by
//! copying a binary to a server, and "is the thing running there the thing I
//! just built?" has to be answerable without guessing — a version string that
//! only changes when someone remembers to bump it cannot answer it.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    watch(Path::new("web"));
    stamp();
}

/// Put the commit and build date in `--version`.
fn stamp() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // A binary built from uncommitted edits must say so, or the commit it
    // names is a lie that is very hard to catch later.
    let dirty = match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(_) => "-dirty",
        None => "",
    };
    let date = git(&["log", "-1", "--format=%cs"]).unwrap_or_else(|| "unknown".into());

    // Recheck when the commit moves, or the stamp names whatever HEAD was the
    // last time some *other* input happened to trigger a rebuild. Watching
    // HEAD and the branch ref covers commit and checkout; deliberately not
    // .git/index, which git rewrites often enough to cause churn.
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=.git/{r}");
    }

    println!("cargo:rustc-env=DUTIME_BUILD={commit}{dirty} {date}");
}

fn watch(dir: &Path) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            watch(&p);
        } else {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    }
}
