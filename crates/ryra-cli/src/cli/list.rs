use anyhow::Result;

pub fn run() -> Result<()> {
    let services = ryra_core::list_installed()?;

    if services.is_empty() {
        println!("No services installed. Run `ryra search` to browse available services.");
        return Ok(());
    }

    println!("{:<20} {:<30} EXPOSURE", "SERVICE", "LOCATION");
    println!("{}", "-".repeat(70));

    for svc in &services {
        let location = match &svc.domain {
            Some(d) => d.clone(),
            None => "-".to_string(),
        };
        println!("{:<20} {:<30} {}", svc.name, location, svc.exposure);
    }

    Ok(())
}
