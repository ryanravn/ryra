use std::collections::BTreeMap;

use base64::Engine;
use minijinja::{Environment, Value};

use crate::error::{Error, Result};

/// Convert SMTP security value to Forgejo's protocol format.
/// starttls → smtp+starttls, force_tls → smtps, off → smtp
fn forgejo_protocol(value: &str) -> String {
    match value {
        "starttls" => "smtp+starttls".to_string(),
        "force_tls" => "smtps".to_string(),
        _ => "smtp".to_string(),
    }
}

/// Convert SMTP security value to Authelia's scheme format.
/// starttls → submission, force_tls → submissions, off → smtp
fn authelia_scheme(value: &str) -> String {
    match value {
        "starttls" => "submission".to_string(),
        "force_tls" => "submissions".to_string(),
        _ => "smtp".to_string(),
    }
}

/// Boolean-as-string flag for "SMTP is plaintext, don't try STARTTLS or TLS".
/// "true" when security is "off", "false" otherwise — matches the shape of
/// env vars like Twenty's EMAIL_SMTP_NO_TLS.
fn smtp_no_tls(value: &str) -> String {
    match value {
        "off" => "true".to_string(),
        _ => "false".to_string(),
    }
}

/// Convert SMTP security value to Ente museum's `encryption` field.
/// Museum treats "tls" as STARTTLS and "ssl" as implicit TLS (SMTPS);
/// an empty value disables encryption. starttls → tls, force_tls → ssl,
/// off → "" (plaintext).
fn ente_smtp_encryption(value: &str) -> String {
    match value {
        "starttls" => "tls".to_string(),
        "force_tls" => "ssl".to_string(),
        _ => String::new(),
    }
}

/// Derive the session-cookie domain Authelia needs from a host. Mirrors the
/// rule the provider expects: `localhost` becomes `127.0.0.1`; a multi-label
/// host (two or more dots, e.g. `auth-host.tailnet.ts.net`) drops its first
/// label so the cookie is valid across sibling subdomains (`tailnet.ts.net`);
/// a single-label or bare host is used as-is. Lives here (not in the
/// generate-config.sh shell) so the value is computed at render time into the
/// `.env`, and Authelia reads it fresh on every start: changing the auth URL
/// no longer leaves a stale cookie domain baked into configuration.yml.
fn cookie_domain(value: &str) -> String {
    if value == "localhost" {
        return "127.0.0.1".to_string();
    }
    if value.matches('.').count() >= 2
        && let Some((_, parent)) = value.split_once('.')
    {
        return parent.to_string();
    }
    value.to_string()
}

/// Render a template string with the given context variables.
///
/// Runs in strict mode: any `{{ foo.bar }}` that isn't in the context errors
/// out instead of rendering as an empty string. Templates that knowingly
/// reference optional context (e.g. `--url` or SMTP not configured) must
/// wrap the reference with the `default` filter:
/// `{{ service.domain | default('localhost') }}`.
pub fn render(template_str: &str, context: &BTreeMap<String, String>) -> Result<String> {
    let mut env = Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);

    // Register custom filters for service-specific derived values.
    // Templates use e.g. `{{ smtp.security | forgejo_protocol }}` instead of
    // the core needing to know about every service's config format.
    env.add_filter("forgejo_protocol", |value: &str| -> String {
        forgejo_protocol(value)
    });
    env.add_filter("authelia_scheme", |value: &str| -> String {
        authelia_scheme(value)
    });
    env.add_filter("smtp_no_tls", |value: &str| -> String {
        smtp_no_tls(value)
    });
    env.add_filter("ente_smtp_encryption", |value: &str| -> String {
        ente_smtp_encryption(value)
    });
    env.add_filter("cookie_domain", |value: &str| -> String {
        cookie_domain(value)
    });
    // Standard-base64 encode a string. Used by services (e.g. Zammad) whose
    // entrypoints expect a base64-encoded JSON payload as an env var.
    env.add_filter("b64encode", |value: &str| -> String {
        base64::engine::general_purpose::STANDARD.encode(value.as_bytes())
    });

    env.add_template("tpl", template_str)
        .map_err(|e| Error::Template(format!("invalid template: {e}")))?;

    let tpl = env
        .get_template("tpl")
        .map_err(|e| Error::Template(e.to_string()))?;

    // Build a nested context from dotted keys: "service.domain" → { service: { domain: val } }
    let ctx = build_nested_context(context);

    tpl.render(&ctx)
        .map_err(|e| Error::Template(format!("render failed: {e}")))
}

fn build_nested_context(flat: &BTreeMap<String, String>) -> Value {
    let mut root: BTreeMap<String, Value> = BTreeMap::new();

    for (key, val) in flat {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() == 1 {
            root.insert(key.clone(), Value::from(val.as_str()));
        } else {
            // For dotted keys, build nested maps
            insert_nested(&mut root, &parts, val);
        }
    }

    Value::from_object(root)
}

fn insert_nested(map: &mut BTreeMap<String, Value>, parts: &[&str], val: &str) {
    if parts.len() == 1 {
        map.insert(parts[0].to_string(), Value::from(val));
        return;
    }

    let key = parts[0].to_string();
    let existing = map.remove(&key);

    // If there's an existing nested map, we need to rebuild it as a BTreeMap
    // so we can insert into it. Since we always build from BTreeMap<String, Value>,
    // we can track this by re-inserting into a fresh map.
    let mut child: BTreeMap<String, Value> = match existing {
        Some(v) => {
            // Try to iterate keys and rebuild
            let mut rebuilt = BTreeMap::new();
            if let Ok(iter) = v.try_iter() {
                for k in iter {
                    let k_str = k.to_string();
                    if let Ok(attr) = v.get_attr(&k_str) {
                        rebuilt.insert(k_str, attr);
                    }
                }
            }
            rebuilt
        }
        None => BTreeMap::new(),
    };

    insert_nested(&mut child, &parts[1..], val);
    map.insert(key, Value::from_object(child));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_four_level_nesting() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut ctx = BTreeMap::new();
        ctx.insert("services.postgres.port.tcp".into(), "5432".into());
        ctx.insert(
            "services.postgres.env.POSTGRES_PASSWORD".into(),
            "secret123".into(),
        );
        ctx.insert("services.postgres.domain".into(), "pg.example.com".into());

        let result = render(
            "postgresql://user:{{ services.postgres.env.POSTGRES_PASSWORD }}@127.0.0.1:{{ services.postgres.port.tcp }}",
            &ctx,
        )?;

        assert_eq!(result, "postgresql://user:secret123@127.0.0.1:5432");
        Ok(())
    }

    #[test]
    fn default_filter_on_missing_key() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut ctx = BTreeMap::new();
        // In real rendering the `service.*` namespace is always populated
        // (service.name, service.port, service.url, …). Individual sub-keys
        // like `service.domain` may still be missing when `--url` wasn't
        // passed, and the `default` filter must handle that.
        ctx.insert("service.name".into(), "whoami".into());
        let result = render("{{ service.domain | default('localhost') }}", &ctx)?;
        assert_eq!(result, "localhost");
        Ok(())
    }

    #[test]
    fn concat_with_default_filter() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Pattern used by services like seafile to fall back to a
        // composed loopback authority when --url isn't set:
        //   {{ service.external_authority | default('127.0.0.1:' ~ service.port) }}
        // Locks in minijinja's `~` string-concat operator so a future
        // crate upgrade can't silently break the fallback.
        let mut ctx = BTreeMap::new();
        ctx.insert("service.port".into(), "10001".into());
        let tpl = "{{ service.external_authority | default('127.0.0.1:' ~ service.port) }}";

        // No external_authority → fallback applies and concatenates.
        assert_eq!(render(tpl, &ctx)?, "127.0.0.1:10001");

        // external_authority set → fallback ignored.
        ctx.insert(
            "service.external_authority".into(),
            "seafile.example.com".into(),
        );
        assert_eq!(render(tpl, &ctx)?, "seafile.example.com");
        Ok(())
    }

    #[test]
    fn strict_mode_rejects_undefined_top_level() {
        let ctx = BTreeMap::new();
        let err = render("{{ bogus_top_level }}", &ctx);
        assert!(
            err.is_err(),
            "expected strict mode to error on an undefined top-level variable"
        );
    }

    #[test]
    fn strict_mode_rejects_typo_without_default() {
        // A realistic typo: smtp.hoist instead of smtp.host. With SMTP
        // configured, `smtp` exists as an object but `hoist` doesn't.
        // Strict mode should surface this at render time rather than
        // silently emitting an empty value.
        let mut ctx = BTreeMap::new();
        ctx.insert("smtp.host".into(), "mail.example.com".into());
        let err = render("{{ smtp.hoist }}", &ctx);
        assert!(
            err.is_err(),
            "expected strict mode to error on a typo'd attribute"
        );
    }

    #[test]
    fn forgejo_protocol_filter() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut ctx = BTreeMap::new();
        ctx.insert("smtp.security".into(), "starttls".into());
        let result = render("{{ smtp.security | forgejo_protocol }}", &ctx)?;
        assert_eq!(result, "smtp+starttls");

        ctx.insert("smtp.security".into(), "force_tls".into());
        let result = render("{{ smtp.security | forgejo_protocol }}", &ctx)?;
        assert_eq!(result, "smtps");

        ctx.insert("smtp.security".into(), "off".into());
        let result = render("{{ smtp.security | forgejo_protocol }}", &ctx)?;
        assert_eq!(result, "smtp");
        Ok(())
    }

    #[test]
    fn ente_smtp_encryption_filter() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut ctx = BTreeMap::new();
        ctx.insert("smtp.security".into(), "starttls".into());
        assert_eq!(
            render("{{ smtp.security | ente_smtp_encryption }}", &ctx)?,
            "tls"
        );

        ctx.insert("smtp.security".into(), "force_tls".into());
        assert_eq!(
            render("{{ smtp.security | ente_smtp_encryption }}", &ctx)?,
            "ssl"
        );

        ctx.insert("smtp.security".into(), "off".into());
        assert_eq!(
            render("{{ smtp.security | ente_smtp_encryption }}", &ctx)?,
            ""
        );
        Ok(())
    }

    #[test]
    fn authelia_scheme_filter() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut ctx = BTreeMap::new();
        ctx.insert("smtp.security".into(), "starttls".into());
        let result = render("{{ smtp.security | authelia_scheme }}", &ctx)?;
        assert_eq!(result, "submission");

        ctx.insert("smtp.security".into(), "force_tls".into());
        let result = render("{{ smtp.security | authelia_scheme }}", &ctx)?;
        assert_eq!(result, "submissions");

        ctx.insert("smtp.security".into(), "off".into());
        let result = render("{{ smtp.security | authelia_scheme }}", &ctx)?;
        assert_eq!(result, "smtp");
        Ok(())
    }

    #[test]
    fn cookie_domain_filter() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let render_domain = |d: &str| -> std::result::Result<String, Box<dyn std::error::Error>> {
            let mut ctx = BTreeMap::new();
            ctx.insert("service.domain".into(), d.into());
            Ok(render("{{ service.domain | cookie_domain }}", &ctx)?)
        };
        // localhost maps to a loopback literal authelia accepts as a cookie domain.
        assert_eq!(render_domain("localhost")?, "127.0.0.1");
        // Multi-label host drops its first label so the cookie spans siblings.
        assert_eq!(
            render_domain("authelia-debian.cobbler-tuna.ts.net")?,
            "cobbler-tuna.ts.net"
        );
        // Single-dot host is used verbatim (e.g. the .internal default).
        assert_eq!(render_domain("auth.internal")?, "auth.internal");
        Ok(())
    }
}
