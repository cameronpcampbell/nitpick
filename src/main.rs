mod cli;
mod rustfmt;

use std::env;
use std::process;

fn main() {
    let arguments = cli::parse_from(env::args_os()).unwrap_or_else(|error| error.exit());
    let code = match rustfmt::run(&arguments) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("nitpick: {error}");

            2
        }
    };

    process::exit(code);
}
