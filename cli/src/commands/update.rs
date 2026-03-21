use anyhow::Result;

use crate::util;

pub async fn run() -> Result<()> {
    util::info(&format!("Current version: {}", env!("CARGO_PKG_VERSION")));

    let spinner = util::spinner("Checking for updates...");

    let status = self_update::backends::github::Update::configure()
        .repo_owner("antkawam")
        .repo_name("claude-code-aws-gateway")
        .bin_name("ccag")
        .current_version(env!("CARGO_PKG_VERSION"))
        .no_confirm(false)
        .show_output(false)
        .build()?
        .update()?;

    spinner.finish_and_clear();

    if status.updated() {
        util::success(&format!("Updated to {}", status.version()));
    } else {
        util::success("Already up to date");
    }

    Ok(())
}
