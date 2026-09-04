//! Thin binary entry point: argv in, exit code out.

use std::env;
use std::process::ExitCode;

use whirl::run;

fn main() -> ExitCode {
    run(env::args_os())
}
