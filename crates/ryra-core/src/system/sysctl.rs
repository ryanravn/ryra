//! Read `net.ipv4.ip_unprivileged_port_start` so the planner can decide
//! whether rootless Caddy can bind ports 80/443 directly. The default on
//! most Linux distros is 1024, which forces ryra to map host 8080→80 and
//! 8443→443 inside the container; bumping the sysctl down to 80 lets
//! rootless processes bind privileged ports without setcap or rootful
//! podman, and Caddy can then listen on 80/443 — clean URLs, simpler
//! router forwarding (no NAT translation).
//!
//! The CLI handles the actual `sudo sysctl` invocation; this module
//! only inspects the current value.

/// Current value of `net.ipv4.ip_unprivileged_port_start`, or `None`
/// when the key doesn't exist (non-Linux) or the value isn't parseable.
/// Treat `None` as "unknown — assume restrictive (high ports)".
///
/// Reads `/proc` directly rather than shelling out to the `sysctl` binary:
/// `sysctl` lives in `/usr/sbin`, which is typically NOT on a rootless user's
/// `$PATH`, so the subprocess silently failed and ryra fell back to high ports
/// (8080/8443) even on a box whose kernel had been tuned to allow 80/443. A
/// plain file read has no `$PATH` dependency and is exactly what `sysctl -n`
/// does under the hood.
pub fn unprivileged_port_start() -> Option<u16> {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Convenience: true when the kernel will let a rootless process bind
/// port 80 and above. Used by `add_service` to decide caddy's host port.
pub fn rootless_can_bind_low_ports() -> bool {
    unprivileged_port_start().is_some_and(|v| v <= 80)
}
