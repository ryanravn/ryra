use anyhow::Result;

pub async fn run(query: Option<&str>, repo: Option<&str>) -> Result<()> {
    let (_repo_url, repo_dir) = ryra_core::resolve_repo(repo).await?;
    let results = ryra_core::search_services(&repo_dir, query)?;

    if results.is_empty() {
        match query {
            Some(q) => println!("No services matching \"{q}\"."),
            None => println!("No services found in repo."),
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
