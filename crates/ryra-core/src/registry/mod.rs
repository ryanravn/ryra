pub mod bundled;
pub mod fetch;
pub mod manage;
pub mod resolve;
pub mod service_def;
pub mod test_def;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use service_def::ServiceDef;

/// Represents a service found in a repo, with its source info.
pub struct RegistryService {
    pub def: ServiceDef,
    /// Path to the service directory (contains service.toml, compose files, etc.)
    pub service_dir: PathBuf,
}

/// Find a service by name in a repo directory.
pub fn find_service(repo_dir: &Path, name: &str) -> Result<RegistryService> {
    let svc_dir = repo_dir.join(name);
    let service_toml = svc_dir.join("service.toml");

    if !service_toml.exists() {
        return Err(Error::ServiceNotFound {
            name: name.to_string(),
            suggestions: suggest_close_names(repo_dir, name),
        });
    }

    let contents = std::fs::read_to_string(&service_toml).map_err(|source| Error::FileRead {
        path: service_toml.clone(),
        source,
    })?;
    let def: ServiceDef = toml::from_str(&contents).map_err(|source| Error::TomlParse {
        path: service_toml,
        source,
    })?;

    if let Err(msg) = def.validate() {
        return Err(Error::ConfigValidation(msg));
    }

    Ok(RegistryService {
        def,
        service_dir: svc_dir,
    })
}

/// List all available services in a repo directory.
pub fn list_available(repo_dir: &Path) -> Result<Vec<RegistryService>> {
    if !repo_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(repo_dir).map_err(|source| Error::FileRead {
        path: repo_dir.to_path_buf(),
        source,
    })?;

    let mut services = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::FileRead {
            path: repo_dir.to_path_buf(),
            source,
        })?;
        let svc_dir = entry.path();
        let service_toml = svc_dir.join("service.toml");
        if service_toml.exists() {
            let contents =
                std::fs::read_to_string(&service_toml).map_err(|source| Error::FileRead {
                    path: service_toml.clone(),
                    source,
                })?;
            let def: ServiceDef = toml::from_str(&contents).map_err(|source| Error::TomlParse {
                path: service_toml,
                source,
            })?;
            services.push(RegistryService {
                def,
                service_dir: svc_dir,
            });
        }
    }

    services.sort_by(|a, b| a.def.service.name.cmp(&b.def.service.name));
    Ok(services)
}

/// Up to three close-match service names from `repo_dir` for a typo'd
/// `name`. Bypasses [`list_available`]'s service.toml parse so we don't
/// fail to suggest just because a sibling service has a malformed file:
/// directory names alone are enough to compare. The Levenshtein
/// threshold is `len/3 + 1` (max 3) so short names get tighter matching
/// — "for" shouldn't match "forgejo" but "forgeo" should.
fn suggest_close_names(repo_dir: &Path, name: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(repo_dir) else {
        return Vec::new();
    };
    let candidates: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("service.toml").exists())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let max_dist = (name.len() / 3 + 1).min(3);
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .map(|c| (levenshtein(name, &c), c))
        .filter(|(d, _)| *d <= max_dist)
        .collect();
    scored.sort_by_key(|(d, _)| *d);
    scored.into_iter().take(3).map(|(_, n)| n).collect()
}

/// Standalone iterative Levenshtein distance — case-insensitive so
/// "Forgejo" vs "forgejo" doesn't add a phantom edit. No dependency,
/// runs in O(n×m) time on rolling vectors.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().flat_map(char::to_lowercase).collect();
    let b: Vec<char> = b.chars().flat_map(char::to_lowercase).collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut dp: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = dp[0];
        dp[0] = i;
        for j in 1..=b.len() {
            let temp = dp[j];
            dp[j] = if a[i - 1] == b[j - 1] {
                prev
            } else {
                1 + prev.min(dp[j].min(dp[j - 1]))
            };
            prev = temp;
        }
    }
    dp[b.len()]
}

/// Render the trailing " — did you mean 'X'?" hint used by
/// [`Error::ServiceNotFound`]. Empty when there are no suggestions, so
/// users with truly unique typos don't see a stray prompt.
pub fn format_service_suggestions(suggestions: &[String]) -> String {
    match suggestions {
        [] => String::new(),
        [one] => format!(" — did you mean '{one}'?"),
        many => format!(" — did you mean one of: {}?", many.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("seafile", "seafile"), 0);
        assert_eq!(levenshtein("seafule", "seafile"), 1); // substitution
        assert_eq!(levenshtein("seafil", "seafile"), 1); // insertion
        assert_eq!(levenshtein("seafiles", "seafile"), 1); // deletion
        assert_eq!(levenshtein("SEAFILE", "seafile"), 0); // case-insensitive
    }

    #[test]
    fn format_suggestions_shapes() {
        assert_eq!(format_service_suggestions(&[]), "");
        assert_eq!(
            format_service_suggestions(&["seafile".into()]),
            " — did you mean 'seafile'?"
        );
        assert_eq!(
            format_service_suggestions(&["seafile".into(), "vikunja".into()]),
            " — did you mean one of: seafile, vikunja?"
        );
    }
}
