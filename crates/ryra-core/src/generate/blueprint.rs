use super::GeneratedFile;

/// Generate an Authentik OIDC blueprint for a service.
///
/// The blueprint declaratively creates an OAuth2/OIDC provider and application
/// in Authentik. Authentik watches `/blueprints/custom/` and auto-applies these
/// on startup / reconciliation — no API calls needed.
pub fn generate_authentik_blueprint(
    service_name: &str,
    domain: &str,
    client_id: &str,
    client_secret: &str,
) -> GeneratedFile {
    let authentik_home = crate::service_home("authentik");
    let path = authentik_home.join("blueprints").join(format!("{service_name}.yaml"));

    // Hand-built YAML because Authentik blueprints use custom tags (!Find, !KeyOf)
    // that standard YAML serializers cannot produce.
    let content = format!(
        r#"version: 1
metadata:
  name: ryra-{service_name}
  labels:
    ryra/managed: "true"
entries:
  - model: authentik_providers_oauth2.oauth2provider
    id: provider-{service_name}
    identifiers:
      name: {service_name}
    attrs:
      name: {service_name}
      authorization_flow: !Find [authentik_flows.flow, [slug, default-provider-authorization-implicit-consent]]
      authentication_flow: !Find [authentik_flows.flow, [slug, default-authentication-flow]]
      client_type: confidential
      client_id: "{client_id}"
      client_secret: "{client_secret}"
      redirect_uris: "https://{domain}/.*"
      property_mappings:
        - !Find [authentik_providers_oauth2.scopemapping, [managed, goauthentik.io/providers/oauth2/scope-openid]]
        - !Find [authentik_providers_oauth2.scopemapping, [managed, goauthentik.io/providers/oauth2/scope-email]]
        - !Find [authentik_providers_oauth2.scopemapping, [managed, goauthentik.io/providers/oauth2/scope-profile]]
  - model: authentik_core.application
    identifiers:
      slug: {service_name}
    attrs:
      name: {service_name}
      slug: {service_name}
      provider: !KeyOf provider-{service_name}
      meta_launch_url: "https://{domain}"
"#
    );

    GeneratedFile { path, content }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blueprint_contains_client_credentials() {
        let bp = generate_authentik_blueprint(
            "affine",
            "affine.example.com",
            "test-client-id",
            "test-client-secret",
        );

        assert!(bp.content.contains("client_id: \"test-client-id\""));
        assert!(bp.content.contains("client_secret: \"test-client-secret\""));
        assert!(bp.content.contains("redirect_uris: \"https://affine.example.com/.*\""));
        assert!(bp.content.contains("name: ryra-affine"));
        assert!(bp.content.contains("slug: affine"));
        assert!(bp.path.ends_with("blueprints/affine.yaml"));
    }

    #[test]
    fn blueprint_is_valid_yaml_structure() {
        let bp = generate_authentik_blueprint(
            "myapp",
            "myapp.example.com",
            "cid",
            "csecret",
        );

        assert!(bp.content.starts_with("version: 1\n"));
        assert!(bp.content.contains("model: authentik_providers_oauth2.oauth2provider"));
        assert!(bp.content.contains("model: authentik_core.application"));
        assert!(bp.content.contains("!Find"));
        assert!(bp.content.contains("!KeyOf"));
    }
}
