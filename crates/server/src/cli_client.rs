//! Client commands (`flicks connect`, `ls`, `get`; FLK-13).

use crate::Error;

#[derive(Debug, clap::Subcommand)]
pub enum Command {}

pub async fn run(command: Command) -> Result<(), Error> {
    match command {}
}
