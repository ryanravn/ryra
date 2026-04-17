/// Names/paths of files and dirs under a service home dir that ryra
/// generates and can regenerate on next `ryra add`. Everything NOT
/// matching this list is treated as user data.
pub const EPHEMERAL_CHILDREN: &[&str] = &[
    ".env",
    "configs",
    "auth-hosts.txt",
];

/// Extensions on top-level home-dir files that are always ephemeral.
pub const EPHEMERAL_EXTENSIONS: &[&str] = &["crt", "sh"];

/// Classify top-level children of a service home dir.
///
/// Returns `(data, ephemeral)`. Both are sorted by path for deterministic
/// output. Entries are absolute paths rooted at `home_dir`.
pub fn classify_home_dir(
    home_dir: &std::path::Path,
) -> std::io::Result<(Vec<std::path::PathBuf>, Vec<std::path::PathBuf>)> {
    let mut data = Vec::new();
    let mut ephemeral = Vec::new();
    if !home_dir.exists() {
        return Ok((data, ephemeral));
    }
    for entry in std::fs::read_dir(home_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if EPHEMERAL_CHILDREN.iter().any(|e| *e == name_str)
            || EPHEMERAL_EXTENSIONS.iter().any(|e| *e == ext)
        {
            ephemeral.push(path);
        } else {
            data.push(path);
        }
    }
    data.sort();
    ephemeral.sort();
    Ok((data, ephemeral))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // Ephemeral children
        fs::write(dir.path().join(".env"), "FOO=bar").unwrap();
        fs::create_dir(dir.path().join("configs")).unwrap();
        fs::write(dir.path().join("ca-bundle.crt"), "x").unwrap();
        fs::write(dir.path().join("resolve-auth-host.sh"), "#!/bin/bash").unwrap();
        fs::write(dir.path().join("auth-hosts.txt"), "x").unwrap();
        // Data children
        fs::create_dir(dir.path().join("shared")).unwrap();
        fs::create_dir(dir.path().join("data")).unwrap();
        fs::write(dir.path().join("shared").join("user-file.bin"), "payload").unwrap();
        dir
    }

    #[test]
    fn classifies_known_ephemerals() {
        let dir = fixture();
        let (data, eph) = classify_home_dir(dir.path()).unwrap();
        let eph_names: Vec<String> = eph
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(eph_names.contains(&".env".to_string()));
        assert!(eph_names.contains(&"configs".to_string()));
        assert!(eph_names.contains(&"ca-bundle.crt".to_string()));
        assert!(eph_names.contains(&"resolve-auth-host.sh".to_string()));
        assert!(eph_names.contains(&"auth-hosts.txt".to_string()));
        let data_names: Vec<String> = data
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(data_names, vec!["data".to_string(), "shared".to_string()]);
    }

    #[test]
    fn missing_home_dir_returns_empty() {
        let (data, eph) = classify_home_dir(std::path::Path::new("/nonexistent-xyz-123")).unwrap();
        assert!(data.is_empty());
        assert!(eph.is_empty());
    }
}
