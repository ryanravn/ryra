use std::io::IsTerminal;

use anyhow::{bail, Result};
use dialoguer::Input;

pub async fn run_add(url: &str, name: Option<&str>) -> Result<()> {
    let name = match name {
        Some(n) => n.to_string(),
        None if std::io::stdin().is_terminal() => Input::new()
            .with_prompt("Registry name")
            .interact_text()?,
        None => bail!("--name is required in non-interactive mode"),
    };

    println!("Adding registry '{name}' from {url}...");
    ryra_core::add_registry(&name, url).await?;
    println!("Registry '{name}' added. Run `ryra list` to see available services.");

    Ok(())
}
