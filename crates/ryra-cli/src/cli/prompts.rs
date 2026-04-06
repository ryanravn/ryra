use anyhow::Result;
use dialoguer::Input;

use ryra_core::config::ConfigPaths;
use ryra_core::config::schema::*;

/// Interactive SMTP setup.
pub fn prompt_smtp() -> Result<Option<SmtpCredentials>> {
    println!();
    let setup = dialoguer::Confirm::new()
        .with_prompt("  Configure SMTP? (for email notifications, password resets)")
        .default(false)
        .interact()?;

    if !setup {
        return Ok(None);
    }

    let host: String = Input::new().with_prompt("SMTP host").interact_text()?;
    let port: u16 = Input::new()
        .with_prompt("SMTP port")
        .default(587)
        .interact_text()?;
    let username: String = Input::new().with_prompt("SMTP username").interact_text()?;
    let password: String = Input::new().with_prompt("SMTP password").interact_text()?;
    let from: String = Input::new()
        .with_prompt("From address")
        .default(format!("noreply@{host}"))
        .interact_text()?;

    Ok(Some(SmtpCredentials {
        host,
        port,
        username,
        password,
        from,
    }))
}

/// What the user chose when prompted for auth setup.
pub enum AuthSetupChoice {
    /// Install a managed Authelia instance via ryra.
    InstallAuthelia,
    /// Use an external OIDC provider (user provided URL).
    External(AuthCredentials),
    /// Skip auth setup.
    Skip,
}

/// Prompt for auth provider configuration.
pub fn prompt_auth() -> Result<AuthSetupChoice> {
    println!();
    println!("  Auth protects your services with single sign-on.");
    println!();

    let items = [
        "authelia — install a managed Authelia instance via ryra",
        "external — use your own OIDC provider (Keycloak, etc.)",
        "skip",
    ];
    let selection = dialoguer::Select::new()
        .with_prompt("Auth provider")
        .items(&items)
        .default(0)
        .interact()?;

    match selection {
        0 => Ok(AuthSetupChoice::InstallAuthelia),
        1 => {
            let url: String = Input::new()
                .with_prompt("OIDC issuer base URL (e.g. https://auth.example.com)")
                .interact_text()?;
            Ok(AuthSetupChoice::External(AuthCredentials::External { url }))
        }
        _ => Ok(AuthSetupChoice::Skip),
    }
}

/// Prompt for auth config, apply if external. Returns true if auth is now configured.
/// For managed authelia, returns false — caller must handle installing authelia.
pub async fn ensure_auth_configured(
    config: &mut Config,
    paths: &ConfigPaths,
) -> Result<AuthSetupChoice> {
    println!();
    println!("  Auth provider not configured yet.");
    let choice = prompt_auth()?;
    if let AuthSetupChoice::External(ref auth) = choice {
        config.auth = Some(auth.clone());
        paths.ensure_dirs()?;
        ryra_core::config::save_config(&paths.config_file, config)?;
        println!("  Config saved to {}", paths.config_file.display());
    }
    Ok(choice)
}
