use anyhow::Result;

pub async fn add(name: &str, url: &str) -> Result<()> {
    println!("Adding registry '{name}' from {url}...");
    ryra_core::registry::manage::add(name, url).await?;
    println!("Registry '{name}' added.");
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    ryra_core::registry::manage::remove(name)?;
    println!("Registry '{name}' removed.");
    Ok(())
}

pub async fn update(name: Option<&str>) -> Result<()> {
    let results = ryra_core::registry::manage::update(name).await?;
    if results.is_empty() {
        println!("No custom registries configured.");
        return Ok(());
    }
    for r in &results {
        println!("{}: updated ({} services)", r.name, r.service_count);
    }
    Ok(())
}

pub fn list() -> Result<()> {
    let registries = ryra_core::registry::manage::list()?;
    if registries.is_empty() {
        println!("No custom registries configured.");
        println!();
        println!("Add one with: ryra registry add <name> <url>");
        return Ok(());
    }
    println!("{:<20} {:<6} URL", "NAME", "SVCS");
    println!("{}", "-".repeat(60));
    for r in &registries {
        println!("{:<20} {:<6} {}", r.name, r.service_count, r.url);
    }
    Ok(())
}
