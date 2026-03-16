use anyhow::Result;
use dialoguer::Input;

pub async fn run(service: &str) -> Result<()> {
    let config = ryra_core::config::load_config(
        &ryra_core::config::ConfigPaths::resolve()?.config_file,
    )?;

    let default_domain = format!("{service}.{}", config.host.domain);
    let domain: String = Input::new()
        .with_prompt(format!("Domain for {service}"))
        .default(default_domain)
        .interact_text()?;

    println!("Adding {service} at {domain}...");
    ryra_core::add_service(service, &domain).await?;
    println!("{service} is running at https://{domain}");

    Ok(())
}
