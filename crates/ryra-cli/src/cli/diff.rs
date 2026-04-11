use anyhow::Result;

pub async fn run(service: &str) -> Result<()> {
    let changes = ryra_core::diff::diff_service(service).await?;

    if changes.is_empty() {
        println!("{service}: up to date (no changes in registry)");
        return Ok(());
    }

    println!("{service}: {} change(s) since install\n", changes.len());
    for change in &changes {
        println!("  {change}");
    }
    println!();

    Ok(())
}
