pub mod add;
pub mod apply;
pub mod info;
pub mod init;
pub mod list;
pub mod registry;
pub mod remove;
pub mod reset;
pub mod search;
pub mod status;

use ryra_core::Step;

/// Print a dry-run summary: files to write, then commands to run.
pub fn print_dry_run(steps: &[Step]) {
    let verbose = ryra_core::verbose::is_enabled();

    let file_steps: Vec<_> = steps
        .iter()
        .filter_map(|s| match s {
            Step::WriteFile(f) => Some(f),
            _ => None,
        })
        .collect();

    let commands: Vec<_> = steps
        .iter()
        .filter(|s| !matches!(s, Step::WriteFile(_)))
        .collect();

    if !file_steps.is_empty() {
        println!("Files to write (sudo required):\n");
        for file in &file_steps {
            println!("  {}", file.path.display());
            if verbose && !file.content.is_empty() {
                for line in file.content.lines() {
                    println!("    | {line}");
                }
                println!();
            }
        }
        if !verbose {
            println!();
        }
    }

    if !commands.is_empty() {
        println!("Commands to run:\n");
        for step in &commands {
            println!("  {}", step.to_command());
        }
        println!();
    }

    println!("Dry run — no changes made. Remove --dry-run to apply.\n");
}
