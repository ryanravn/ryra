use anyhow::Result;
use ryra_core::ServiceStatus;

pub fn run() -> Result<()> {
    let services = ryra_core::list_services()?;

    if services.is_empty() {
        println!("No services found. Add a registry with `ryra registry add <url>`.");
        return Ok(());
    }

    println!("{:<20} {:<10} {}", "SERVICE", "STATUS", "DETAILS");
    println!("{}", "-".repeat(60));

    for svc in &services {
        match svc {
            ServiceStatus::Available { name, description } => {
                println!("{:<20} {:<10} {}", name, "available", description);
            }
            ServiceStatus::Installed {
                name,
                description,
                domain,
            } => {
                println!("{:<20} {:<10} {} ({})", name, "installed", description, domain);
            }
        }
    }

    Ok(())
}
