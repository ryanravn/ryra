use anyhow::Result;
use dialoguer::Input;

pub async fn run_add(url: &str) -> Result<()> {
    let name: String = Input::new()
        .with_prompt("Registry name")
        .interact_text()?;

    println!("Adding registry '{name}' from {url}...");
    ryra_core::add_registry(&name, url).await?;
    println!("Registry '{name}' added. Run `ryra list` to see available services.");

    Ok(())
}
