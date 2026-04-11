use anyhow::Result;
use ryra_core::registry::resolve::ServiceRef;

pub async fn run(query: Option<&str>, registry: Option<&str>) -> Result<()> {
    let repo_dir = match registry {
        Some(name) => {
            let service_ref = ServiceRef::Custom {
                registry: name.to_string(),
                service: String::new(),
            };
            ryra_core::resolve_registry_dir(&service_ref).await?
        }
        None => {
            let service_ref = ServiceRef::Bundled(String::new());
            ryra_core::resolve_registry_dir(&service_ref).await?
        }
    };
    let results = ryra_core::search_services(&repo_dir, query)?;

    if results.is_empty() {
        match query {
            Some(q) => println!("No services matching \"{q}\"."),
            None => println!("No services found in registry."),
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
