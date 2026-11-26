//! The Satella terminal harness.
//!
//! The command line lives in the library, so that the checks can reach the
//! same code the binary runs.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    satella_cli::run(satella_cli::parse_args()?).await
}
