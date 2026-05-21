use anyhow::Result;
use ryra_core::system::doctor::{Issue, Severity, check_all};

pub fn run() -> Result<()> {
    let paths = ryra_core::config::ConfigPaths::resolve()?;
    let config = ryra_core::config::load_or_default(&paths.config_file)?;
    let issues = check_all(&config);

    if issues.is_empty() {
        println!("No issues found.");
        return Ok(());
    }

    let mut blockers = 0;
    let mut warnings = 0;
    let mut infos = 0;
    for i in &issues {
        match i.severity() {
            Severity::Blocker => blockers += 1,
            Severity::Warning => warnings += 1,
            Severity::Info => infos += 1,
        }
    }

    println!(
        "{} issue{} found ({blockers} blocker{}, {warnings} warning{}, {infos} info).\n",
        issues.len(),
        plural(issues.len()),
        plural(blockers),
        plural(warnings),
    );

    print_section(&issues, Severity::Blocker, "blocker");
    print_section(&issues, Severity::Warning, "warning");
    print_section(&issues, Severity::Info, "info");

    if blockers > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn print_section(issues: &[Issue], sev: Severity, label: &str) {
    let filtered: Vec<&Issue> = issues.iter().filter(|i| i.severity() == sev).collect();
    if filtered.is_empty() {
        return;
    }
    for issue in filtered {
        println!("[{label}] {issue}\n");
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
