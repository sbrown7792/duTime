//! Make asset edits trigger a rebuild.
//!
//! `rust-embed` bakes `web/` into the binary at compile time, but Cargo has no
//! idea those files are inputs — so editing the UI and rebuilding silently
//! ships the *previous* assets. That failure is near-invisible: the binary
//! builds, the server starts, and you debug a change that was never deployed.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    watch(Path::new("web"));
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
