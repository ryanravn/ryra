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

    println!("{:<20} {:<10} DESCRIPTION", "SERVICE", "STATUS");
    println!("{}", "-".repeat(70));

    for svc in &results {
        let status = if svc.installed { "installed" } else { "" };
        println!("{:<20} {:<10} {}", svc.name, status, svc.description);
    }

    Ok(())
}
