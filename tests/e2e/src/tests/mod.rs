use crate::scenario::Scenario;

/// All registered test scenarios.
///
/// Each scenario gets its own QEMU VM — a fresh Debian install with
/// podman, nginx, and systemd ready to go. Add new scenarios by
/// appending to this list.
pub fn all() -> Vec<Scenario> {
    vec![
        // --- Init ---
        Scenario::new("init").assert_file_exists("/etc/ryra/ryra.toml"),
        Scenario::new("idempotent-init")
            .assert_file_exists("/etc/ryra/ryra.toml")
            .assert_config_contains("default_repo"),
        // --- Single service ---
        Scenario::new("add-whoami")
            .add("whoami")
            .assert_running("whoami")
            .assert_user_exists("ryra-whoami")
            .assert_http("whoami", 200)
            .assert_journal_clean("whoami")
            .assert_config_contains("whoami"),
        // --- Lifecycle ---
        Scenario::new("remove-whoami")
            .add("whoami")
            .assert_running("whoami")
            .remove("whoami")
            .assert_not_running("whoami")
            .assert_user_not_exists("ryra-whoami")
            .assert_config_not_contains("whoami"),
        Scenario::new("reset")
            .add("whoami")
            .reset()
            .assert_not_running("whoami")
            .assert_user_not_exists("ryra-whoami")
            .assert_file_not_exists("/etc/ryra/ryra.toml"),
        // --- Infrastructure ---
        Scenario::new("add-postgres")
            .add("postgres")
            .assert_running("postgres")
            .assert_user_exists("ryra-postgres")
            .assert_journal_clean("postgres")
            .assert_config_contains("postgres"),
        // --- Multi-service ---
        Scenario::new("whoami-plus-postgres")
            .add("whoami")
            .add("postgres")
            .assert_running("whoami")
            .assert_running("postgres")
            .assert_http("whoami", 200)
            .assert_config_contains("whoami")
            .assert_config_contains("postgres"),
        // --- Re-add after remove ---
        Scenario::new("re-add-after-remove")
            .add("whoami")
            .assert_running("whoami")
            .remove("whoami")
            .assert_not_running("whoami")
            .add("whoami")
            .assert_running("whoami")
            .assert_http("whoami", 200),
    ]
}
