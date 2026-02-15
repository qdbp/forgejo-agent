// Thin crate entrypoint for the `orchd` binary.
//
// All real logic lives under `src/bin/orchd/` so the binary root stays small.

#[path = "orchd/mod.rs"]
mod orchd;

fn main() {
    if let Err(err) = orchd::run_entry() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}
