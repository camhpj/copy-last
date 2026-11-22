//! cl: copy your last terminal commands and their output.
//!
//! A terminal doesn't record which output belongs to which command, so
//! `cl shell` runs your shell inside a pseudo-terminal. Shell hooks print
//! invisible markers before and after each command. `cl shell` strips them
//! from the stream and stores each command with its output.
//! `cl -n N -s SKIP` then copies from that per-session history.

#[cfg(not(unix))]
compile_error!("cl only supports Unix-like systems (macOS and Linux)");

mod clipboard;
mod copy;
mod recorder;
mod shell;
mod store;
mod term;

use std::process::ExitCode;

use clap::builder::RangedU64ValueParser;
use clap::{Args, Parser, Subcommand, ValueEnum};

const ZSH_HOOKS: &str = include_str!("hooks/cl.zsh");
const BASH_HOOKS: &str = include_str!("hooks/cl.bash");

#[derive(Parser)]
#[command(
    name = "cl",
    version,
    about = "Copy your last terminal commands and their output",
    long_about = "Copy your last terminal commands and their output.\n\n\
                  cl records the commands that you run in `cl shell`. Add\n\
                  `eval \"$(cl init zsh)\"` (or `cl init bash`) to your rc file once.\n\
                  Then run `cl shell` to start a session.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    copy: CopyArgs,
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)] // they're independent CLI flags
struct CopyArgs {
    /// Number of commands to copy
    #[arg(short = 'n', long, default_value_t = 1, value_parser = at_least_one())]
    count: usize,
    /// Skip this many of the most recent commands first
    #[arg(short, long, default_value_t = 0)]
    skip: usize,
    /// Copy only the command lines
    #[arg(short, long, conflicts_with = "output_only")]
    commands_only: bool,
    /// Copy only the output
    #[arg(short, long)]
    output_only: bool,
    /// Print to stdout instead of copying
    #[arg(short, long)]
    print: bool,
    /// Copy with the OSC 52 escape sequence instead of a clipboard tool
    #[arg(long)]
    osc52: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a shell that records commands and their output
    Shell,
    /// Print the shell hook for your rc file, for example `eval "$(cl init zsh)"`
    Init { shell: ShellKind },
    /// List recent commands, with the number to pass to --skip
    List {
        /// Number of commands to list
        #[arg(short = 'n', long, default_value_t = 20, value_parser = at_least_one())]
        count: usize,
    },
}

#[derive(Clone, ValueEnum)]
enum ShellKind {
    Zsh,
    Bash,
}

fn at_least_one() -> RangedU64ValueParser<usize> {
    RangedU64ValueParser::new().range(1..)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Some(Cmd::Shell) => shell::run().map(|never| match never {}),
        Some(Cmd::Init { shell }) => {
            print!(
                "{}",
                match shell {
                    ShellKind::Zsh => ZSH_HOOKS,
                    ShellKind::Bash => BASH_HOOKS,
                }
            );
            Ok(())
        }
        Some(Cmd::List { count }) => copy::list(count),
        None => copy::copy(&copy::Selection {
            count: cli.copy.count,
            skip: cli.copy.skip,
            commands_only: cli.copy.commands_only,
            output_only: cli.copy.output_only,
            print: cli.copy.print,
            osc52: cli.copy.osc52,
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cl: {e:#}");
            ExitCode::FAILURE
        }
    }
}
