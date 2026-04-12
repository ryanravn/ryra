use anyhow::Result;

pub fn run() -> Result<()> {
    let services = ryra_core::list_installed()?;

    if services.is_empty() {
        println!("No services installed. Run `ryra search` to browse available services.");
        return Ok(());
    }

    println!("{:<20} REPO", "SERVICE");
    println!("{}", "-".repeat(50));

    for svc in &services {
        let label = if svc.installed {
            svc.name.clone()
        } else {
            format!("{} (incomplete)", svc.name)
        };
        println!("{:<20} {}", label, svc.repo);
    }

    Ok(())
}
