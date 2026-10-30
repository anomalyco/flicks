//! Running at login (`flicks service install|uninstall`; FLK-16).

use crate::Error;

#[derive(Debug, clap::Subcommand)]
pub enum Command {}

pub async fn run(command: Command) -> Result<(), Error> {
    match command {}
}
