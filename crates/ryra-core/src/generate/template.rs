use std::collections::BTreeMap;

use minijinja::{Environment, Value};

use crate::error::{Error, Result};

/// Render a template string with the given context variables.
pub fn render(template_str: &str, context: &BTreeMap<String, String>) -> Result<String> {
    let mut env = Environment::new();
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
    fn render_four_level_nesting() {
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
        ).unwrap();

        assert_eq!(result, "postgresql://user:secret123@127.0.0.1:5432");
    }
}
