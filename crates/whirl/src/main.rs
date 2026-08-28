//! Thin binary entry point: argv in, exit code out.

use std::env;
use std::process::ExitCode;

use whirl::cli;

fn main() -> ExitCode {
    cli::run(env::args_os())
}
