use nitpick::{cli, format, rustfmt};
use std::env;
use std::process;

fn main() {
    let invocation = cli::invocation(env::args_os()).unwrap_or_else(|error| error.exit());

    let code = match invocation {
        cli::Invocation::Rustfmt(arguments) => rustfmt::run(&arguments).unwrap_or_else(|error| {
            eprintln!("nitpick: {error}");

            2
        }),

        cli::Invocation::Format(options) => match format::run(&options) {
            Ok(code) => code,
            Err(format::Failure::Child(code)) => code,

            Err(error) => {
                eprintln!("nitpick: {error}");

                2
            }
        },
    };

    process::exit(code);
}
