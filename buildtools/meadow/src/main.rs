//! The `meadow` command-line entry point. The command line itself is the
//! library's [`meadow::cli`], so that MeadowBoot can run it in its own
//! process through `meadow-host`; this binary only hands it the arguments.

fn main() {
    std::process::exit(meadow::cli::main(std::env::args_os()));
}
