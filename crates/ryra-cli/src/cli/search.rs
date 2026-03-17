use anyhow::Result;

pub fn run(query: Option<&str>) -> Result<()> {
    let results = ryra_core::search_services(query)?;

    if results.is_empty() {
        match query {
            Some(q) => println!("No services matching \"{q}\"."),
            None => println!("No services found. Add a registry with `ryra registry add <url>`."),
        }
        return Ok(());
    }

    println!("{:<20} {:<6} {:<10} DESCRIPTION", "SERVICE", "TYPE", "STATUS");
    println!("{}", "-".repeat(75));

    for svc in &results {
        let svc_type = if svc.is_web { "web" } else { "tcp" };
        let status = if svc.installed { "installed" } else { "-" };
        println!(
            "{:<20} {:<6} {:<10} {}",
            svc.name, svc_type, status, svc.description
        );
    }

    Ok(())
}
