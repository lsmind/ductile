//! Ductile CLI — entry point.

use std::env;
use std::process::exit;

fn main() {
    let args: Vec<String> = env::args().collect();
    let exit_code = ductile::cli::run(&args).unwrap_or_else(|e| {
        eprintln!("{}", e);
        1
    });
    exit(exit_code);
}
