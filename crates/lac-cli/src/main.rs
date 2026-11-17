//! `lac`: command line front end of losslessaudiocut. Everything but the
//! process exit lives in the library (`src/lib.rs`), which the Python
//! package shares.

fn main() {
    std::process::exit(lac_cli::run(std::env::args_os()));
}
