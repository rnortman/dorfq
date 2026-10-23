//! The `dorfq` command line. A placeholder with no subcommands yet.

use clap::Parser;

#[derive(Parser)]
#[command(name = "dorfq", version = dorfq::version())]
struct Cli {}

fn main() {
    Cli::parse();
    println!("dorfq {}", dorfq::version());
}
