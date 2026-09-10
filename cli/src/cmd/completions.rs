//! `wattson completions` — print a shell completion script.

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::Shell;

use crate::exit;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Which shell to generate for.
    #[arg(value_enum)]
    pub shell: Shell,
}

pub fn run<C: CommandFactory>(args: &Args) -> Result<i32> {
    let mut cmd = C::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(args.shell, &mut cmd, name, &mut std::io::stdout());
    Ok(exit::SUCCESS)
}
