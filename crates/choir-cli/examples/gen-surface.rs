//! Writes every generated description of the agent surface.
//!
//! ```text
//! cargo run -p choir-cli --example gen-surface
//! ```
//!
//! The table in `surface.rs` is the source; this only puts it on disk.
//! `tests/it/surface.rs` re-renders and compares, so a stale artifact fails
//! the suite rather than being noticed by a reader months later.

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let artifacts = match choir_cli::surface::artifacts(&root) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    for (path, contents) in artifacts {
        let changed = std::fs::read_to_string(&path)
            .map(|old| old != contents)
            .unwrap_or(true);
        if changed {
            std::fs::write(&path, &contents).expect("write artifact");
            println!("wrote {}", path.display());
        } else {
            println!("unchanged {}", path.display());
        }
    }
}
