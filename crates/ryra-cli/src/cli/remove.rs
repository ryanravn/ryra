use anyhow::Result;
use dialoguer::Confirm;

pub async fn run(service: &str) -> Result<()> {
    let confirmed = Confirm::new()
        .with_prompt(format!("Remove {service}? This will stop the service and delete its quadlet files."))
        .default(false)
        .interact()?;

    if !confirmed {
        println!("Cancelled.");
        return Ok(());
    }

    println!("Removing {service}...");
    ryra_core::remove_service(service).await?;
    println!("{service} removed.");

    Ok(())
}
