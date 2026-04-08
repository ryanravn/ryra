use std::collections::BTreeMap;

use crate::config::schema::AuthCredentials;
use crate::generate::GeneratedFile;
use crate::registry::service_def::ServiceDef;
use crate::{Step, service_home};

/// Register an OIDC client with authelia by editing its configuration.yml.
/// Returns steps to write the updated config and restart authelia.
pub fn register_oidc_client(
    service_name: &str,
    service_def: &ServiceDef,
    domain: Option<&str>,
    ctx: &BTreeMap<String, String>,
) -> Vec<Step> {
    let mut steps = Vec::new();

    let client_id = match ctx.get("auth.client_id") {
        Some(id) => id.clone(),
        None => return steps,
    };
    let client_secret = match ctx.get("auth.client_secret") {
        Some(s) => s.clone(),
        None => return steps,
    };

    let authelia_config_dir = service_home("authelia").join("config");
    let authelia_config_path = authelia_config_dir.join("configuration.yml");
    let rsa_key_path = authelia_config_dir.join("oidc.jwk.rsa.pem");

    // Generate RSA key if not exists (for OIDC JWKS)
    if !rsa_key_path.exists() {
        let _ = std::process::Command::new("podman")
            .args([
                "run",
                "--rm",
                "-v",
                &format!("{}:/out:Z", authelia_config_dir.display()),
                "docker.io/authelia/authelia:4.39",
                "authelia",
                "crypto",
                "pair",
                "rsa",
                "generate",
                "--directory",
                "/out",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::fs::rename(authelia_config_dir.join("private.pem"), &rsa_key_path);
    }

    // Add OIDC section + client to authelia config
    if !authelia_config_path.exists() {
        return steps;
    }
    let Ok(mut yaml) = std::fs::read_to_string(&authelia_config_path) else {
        return steps;
    };

    let service_url = ctx.get("service.url").cloned().unwrap_or_default();
    let base_url = match domain {
        Some(d) => format!("https://{d}:8443"),
        None => service_url,
    };

    // Build redirect_uris from mappings and common callback paths
    let redirect_url_from_mappings = service_def
        .mappings
        .auth
        .get("OAUTH_REDIRECT_URL")
        .map(|v| {
            v.replace("{{service.url}}", &base_url)
                .replace("{{service.external_url}}", &base_url)
        });
    let mut redirect_uris = Vec::new();
    if let Some(ref url) = redirect_url_from_mappings {
        redirect_uris.push(url.clone());
    }
    for suffix in [
        "/user/oauth2/Authelia/callback", // Forgejo/Gitea
        "/auth/login",                    // Immich
        "/oauth2/callback",               // generic
    ] {
        let uri = format!("{base_url}{suffix}");
        if !redirect_uris.contains(&uri) {
            redirect_uris.push(uri);
        }
    }
    let redirect_uris_yaml: String = redirect_uris
        .iter()
        .map(|u| format!("\n          - '{u}'"))
        .collect();
    let client_block = format!(
        "\n      - client_id: '{client_id}'\n        client_name: '{service_name}'\n        client_secret: '{client_secret}'\n        redirect_uris:{redirect_uris_yaml}\n        scopes:\n          - 'openid'\n          - 'email'\n          - 'profile'\n        authorization_policy: 'one_factor'"
    );

    if !yaml.contains("identity_providers:") {
        yaml.push_str(&format!(
            "\nidentity_providers:\n  oidc:\n    jwks:\n      - key_id: 'main'\n        algorithm: 'RS256'\n        use: 'sig'\n        key: {{{{ secret \"/config/oidc.jwk.rsa.pem\" | mindent 10 \"|\" | msquote }}}}\n    clients:{client_block}\n",
        ));
    } else if !yaml.contains(&client_id) {
        yaml = yaml.replace("    clients:", &format!("    clients:{client_block}"));
    }

    steps.push(Step::WriteFile(GeneratedFile {
        path: authelia_config_path,
        content: yaml,
    }));

    steps.push(Step::RestartService {
        unit: "authelia".to_string(),
    });

    steps
}

/// Build AuthCredentials for finalize_add when authelia is installed.
pub fn auth_config(allocated_ports: &[(String, u16)]) -> AuthCredentials {
    let port = allocated_ports
        .iter()
        .find(|(name, _)| name == "http")
        .map(|(_, p)| *p)
        .unwrap_or(9091);
    let url = format!("http://localhost:{port}");
    AuthCredentials::Authelia { url, port }
}
