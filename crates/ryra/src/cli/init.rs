//! `ryra init`: scaffold a `service.toml` (+ `test.toml`) in the current
//! project. Additive like `git init`: detects the project type to infer the
//! run/build commands, prompts for name + port (blank allowed), and writes the
//! files. Never touches your source.

use std::path::Path;

use anyhow::{Context, Result, bail};
use console::style;
use dialoguer::Input;

/// What kind of project we're in. Drives the inferred `run`/`build` commands.
enum Project {
    /// Rust crate (Cargo.toml). `crate_name` is `[package].name` if readable.
    Rust { crate_name: Option<String> },
    /// JS/TS project (package.json). Run with Bun.
    Bun,
    /// Neither: the user has to fill in `run` themselves.
    Unknown,
}

pub fn run(name_flag: Option<&str>, port_flag: Option<u16>, yes: bool) -> Result<()> {
    let dir = std::env::current_dir().context("cannot resolve current directory")?;
    let service_toml = dir.join("service.toml");
    if service_toml.exists() {
        bail!(
            "service.toml already exists in {} (refusing to overwrite)",
            dir.display()
        );
    }

    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("service")
        .to_string();
    let project = detect(&dir);

    println!(
        "Detected {}.",
        match &project {
            Project::Rust { .. } => "a Rust project (Cargo.toml)",
            Project::Bun => "a Bun project (package.json)",
            Project::Unknown => "no Cargo.toml or package.json",
        }
    );

    let interactive = super::is_interactive() && !yes;

    // name: --name, else crate name, else directory name.
    let default_name = name_flag
        .map(str::to_string)
        .unwrap_or_else(|| match &project {
            Project::Rust {
                crate_name: Some(c),
            } => c.clone(),
            _ => dir_name.clone(),
        });
    let name = prompt(interactive, "Service name", default_name)?;
    // Description isn't prompted: default it to the name; the user edits the
    // file if they want something richer.
    let description = name.clone();

    // Port: --port wins; else prompt (blank is allowed and means "fill in
    // later", written as a visible-but-unset placeholder). Non-interactive with
    // no --port leaves the placeholder too.
    let port: Option<u16> = if let Some(p) = port_flag {
        Some(p)
    } else if interactive {
        let raw: String = Input::new()
            .with_prompt("HTTP port (blank to fill in later)")
            .allow_empty(true)
            .interact_text()?;
        let raw = raw.trim();
        if raw.is_empty() {
            None
        } else {
            Some(raw.parse().context("port must be a number")?)
        }
    } else {
        None
    };

    // Inferred run/build per project type.
    let (build, run) = match &project {
        Project::Rust { crate_name } => {
            let bin = crate_name.clone().unwrap_or_else(|| name.clone());
            (
                Some("cargo build --release".to_string()),
                format!("target/release/{bin}"),
            )
        }
        Project::Bun => (
            Some("bun install".to_string()),
            "bun --watch run src/index.ts".to_string(),
        ),
        Project::Unknown => (None, "./REPLACE-WITH-YOUR-RUN-COMMAND".to_string()),
    };

    std::fs::write(
        &service_toml,
        render_service_toml(&name, &description, build.as_deref(), &run, port),
    )
    .with_context(|| format!("writing {}", service_toml.display()))?;

    // Don't clobber an existing test.toml.
    let test_toml = dir.join("test.toml");
    if !test_toml.exists() {
        std::fs::write(&test_toml, render_test_toml(&name))
            .with_context(|| format!("writing {}", test_toml.display()))?;
    }

    println!();
    println!(
        "{} service.toml{}",
        style("Wrote").green().bold(),
        if test_toml.exists() {
            " + test.toml"
        } else {
            ""
        }
    );
    if matches!(project, Project::Unknown) {
        println!(
            "  {} set {} in service.toml to your start command first",
            style("!").yellow().bold(),
            style("run").bold()
        );
    }
    println!("Next: {}", style("ryra add").bold());
    Ok(())
}

fn detect(dir: &Path) -> Project {
    if dir.join("Cargo.toml").is_file() {
        let crate_name = std::fs::read_to_string(dir.join("Cargo.toml"))
            .ok()
            .and_then(|s| s.parse::<toml::Value>().ok())
            .and_then(|v| v.get("package")?.get("name")?.as_str().map(str::to_string));
        Project::Rust { crate_name }
    } else if dir.join("package.json").is_file() {
        Project::Bun
    } else {
        Project::Unknown
    }
}

fn prompt(interactive: bool, label: &str, default: String) -> Result<String> {
    if interactive {
        Ok(Input::new()
            .with_prompt(label)
            .default(default)
            .interact_text()?)
    } else {
        Ok(default)
    }
}

const DOCS_URL: &str = "https://ryra.dev";

fn render_service_toml(
    name: &str,
    description: &str,
    build: Option<&str>,
    run: &str,
    port: Option<u16>,
) -> String {
    let mut s = format!("# {DOCS_URL}\n[service]\n");
    s.push_str(&format!("name = {}\n", q(name)));
    s.push_str(&format!("description = {}\n", q(description)));
    s.push_str("runtime = \"native\"\n");
    if let Some(b) = build {
        s.push_str(&format!("build = {}\n", q(b)));
    }
    s.push_str(&format!("run = {}\n", q(run)));
    match port {
        Some(p) => {
            s.push_str(&format!(
                "\n[[ports]]\nname = \"http\"\ncontainer_port = {p}\n"
            ));
        }
        None => {
            // Blank port: keep the block visible with the field present but
            // unset (0), so it's obvious a port belongs here and needs filling
            // in. `ryra add` refuses to install until it's a real port.
            s.push_str(
                "\n[[ports]]\nname = \"http\"\ncontainer_port = 0  # fill in the port your service listens on\n",
            );
        }
    }
    s
}

fn render_test_toml(name: &str) -> String {
    format!(
        "[[tests]]\n\
         name = {n}\n\
         [[tests.steps]]\n\
         action = \"add\"\n\
         service = {n}\n\
         timeout = 120\n\
         [[tests.steps]]\n\
         action = \"wait\"\n\
         service = {n}\n",
        n = q(name)
    )
}

/// Quote a value as a TOML basic string (escapes `\` and `"`).
fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
