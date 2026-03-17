use anyhow::Result;

pub fn run(service: &str) -> Result<()> {
    let detail = ryra_core::service_info(service)?;

    println!("{}", detail.name);
    println!("  {}", detail.description);

    if let Some(url) = &detail.url {
        println!("  Docs: {url}");
    }

    if detail.is_compose {
        println!("  Deploy: compose (multi-container)");
    }

    if !detail.ports.is_empty() {
        println!();
        println!("Ports:");
        for (port, proto, name) in &detail.ports {
            println!("  {name}: {port}/{proto}");
        }
    }

    let configurable: Vec<_> = detail
        .env_vars
        .iter()
        .filter(|(_, prompt)| prompt.is_some())
        .collect();
    if !configurable.is_empty() {
        println!();
        println!("Configuration (prompted during add):");
        for (name, prompt) in &configurable {
            println!(
                "  {name}: {}",
                prompt.as_deref().unwrap_or("")
            );
        }
    }

    if let Some(exposure) = &detail.installed_exposure {
        println!();
        match &detail.installed_domain {
            Some(domain) => println!("Installed: {domain} ({exposure})"),
            None => println!("Installed: ({exposure})"),
        }
    }

    Ok(())
}
