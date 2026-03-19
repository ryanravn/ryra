use std::collections::BTreeMap;
use std::path::PathBuf;

/// Test that the full pipeline works:
/// 1. Read service.toml files from the real registry/
/// 2. Serialize to JSON (like generate-registry does)
/// 3. Deserialize JSON back into JsonRegistry
/// 4. Write as individual service.toml files to a temp dir
/// 5. Read those files back with ryra-core's find_service/list_available
/// 6. Verify all services survived the round-trip with correct data
#[test]
fn json_registry_roundtrip() {
    let registry_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../registry");

    // 1. Load all services from TOML files
    let original_services = ryra_core::registry::list_available(&registry_dir)
        .expect("failed to list services from registry/");
    assert!(!original_services.is_empty(), "registry has no services");

    // 2. Build the JSON structure (same as generate-registry binary)
    let map: BTreeMap<String, ryra_core::registry::service_def::ServiceDef> = original_services
        .iter()
        .map(|s| (s.def.service.name.clone(), s.def.clone()))
        .collect();

    let registry = ryra_core::registry::fetch::JsonRegistry { services: map };

    let json = serde_json::to_string_pretty(&registry)
        .expect("failed to serialize to JSON");

    // 3. Parse JSON back
    let parsed: ryra_core::registry::fetch::JsonRegistry =
        serde_json::from_str(&json).expect("failed to parse JSON back");

    assert_eq!(
        parsed.services.len(),
        original_services.len(),
        "JSON round-trip lost services"
    );

    // 4. Write to temp directory as service.toml files
    let tmp_dir = std::env::temp_dir().join("ryra-test-json-roundtrip");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).unwrap();

    ryra_core::registry::fetch::write_json_registry_to_dir(&parsed, &tmp_dir)
        .expect("failed to write JSON registry to dir");

    // 5. Read back with ryra-core's standard functions
    let roundtripped = ryra_core::registry::list_available(&tmp_dir)
        .expect("failed to list services from roundtripped dir");

    // 6. Verify
    assert_eq!(
        roundtripped.len(),
        original_services.len(),
        "roundtripped service count mismatch"
    );

    for original in &original_services {
        let name = &original.def.service.name;

        // find_service should work
        let found = ryra_core::registry::find_service(&tmp_dir, name)
            .unwrap_or_else(|_| panic!("service '{name}' not found after roundtrip"));

        // Key fields match
        assert_eq!(found.def.service.name, original.def.service.name);
        assert_eq!(found.def.service.description, original.def.service.description);
        assert_eq!(found.def.ports.len(), original.def.ports.len(), "port count mismatch for {name}");
        assert_eq!(found.def.volumes.len(), original.def.volumes.len(), "volume count mismatch for {name}");
        assert_eq!(found.def.env.len(), original.def.env.len(), "env count mismatch for {name}");

        // Requirements survived
        match (&original.def.requirements, &found.def.requirements) {
            (Some(orig_req), Some(found_req)) => {
                assert_eq!(orig_req.ram.min, found_req.ram.min, "ram min mismatch for {name}");
                assert_eq!(orig_req.ram.recommended, found_req.ram.recommended, "ram recommended mismatch for {name}");
            }
            (None, None) => {}
            _ => panic!("requirements mismatch for {name}"),
        }
    }

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
