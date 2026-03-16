use anyhow::Result;
use dialoguer::Input;

use ryra_core::config::schema::*;

pub async fn run() -> Result<()> {
    println!("Welcome to ryra! Let's set up your server.\n");

    let domain: String = Input::new()
        .with_prompt("Your domain (e.g., example.com)")
        .interact_text()?;

    let data_dir: String = Input::new()
        .with_prompt("Data directory for service volumes")
        .default("/srv/ryra".into())
        .interact_text()?;

    let registry_url: String = Input::new()
        .with_prompt("Registry URL (git repo or local path)")
        .default("https://github.com/user/ryra-registry".into())
        .interact_text()?;

    let config = Config {
        host: HostConfig { domain, data_dir },
        dns: DnsConfig::None,
        ssl: SslConfig::None,
        smtp: SmtpConfig::None,
        auth: AuthConfig::None,
        registries: vec![RegistryEntry {
            name: "default".into(),
            url: registry_url,
        }],
        services: vec![],
    };

    ryra_core::init(config).await?;

    println!("\nryra initialized! Run `ryra list` to see available services.");
    Ok(())
}
