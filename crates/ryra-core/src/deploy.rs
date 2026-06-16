//! Blue/green deploy: the runtime-agnostic step sequence for a zero-downtime
//! color swap.
//!
//! A blue/green service runs two interchangeable slots, `blue` and `green`, on
//! their own ports. At any moment one is *live* (Caddy routes to it) and the
//! other is idle. A deploy readies the new version on the idle slot, proves it
//! healthy, swaps the Caddy upstream over with a graceful reload (no dropped
//! connections), then stops the old slot. Because the old slot lingers through
//! the swap, rollback is just the reverse upstream swap — no rebuild.
//!
//! The *sequence* is identical regardless of how a slot is realized — that's
//! the whole point. A slot is "an immutable artifact + a port", and the two
//! runtimes only differ in how that artifact is produced:
//!
//! - **podman**: the artifact is an image; readying the idle slot is a
//!   [`Step::PullImage`], and each color is its own quadlet/container. Immutable
//!   for free, so this is the baseline and covers every language.
//! - **native**: the artifact is the idle color's own working dir (a synced,
//!   separately-built copy of the source), so a Python/C++/Node/Rust process
//!   keeps serving from its slot while the new code builds in the other. Readying
//!   the idle slot is a [`Step::Build`] in that slot's dir.
//!
//! Either way the swap below is the same five moves, which is why this lives in
//! one runtime-agnostic builder.

use crate::Step;
use crate::registry::service_def::Color;

/// systemd unit name (without the `.service` suffix) for one color slot.
///
/// Native units and podman quadlet-units both follow `<service>-<color>`, so
/// the swap plan never has to branch on runtime to name what it starts and
/// stops. Mirrors how a single-instance service's unit is just `<service>`.
pub fn color_unit(service_name: &str, color: Color) -> String {
    format!("{service_name}-{color}")
}

/// Quadlet filename for one color slot: `<service>-<color>.container`.
/// systemd's generator turns that into the `<service>-<color>.service` unit
/// that [`color_unit`] names, so the two stay in lockstep.
pub fn color_quadlet_filename(service_name: &str, color: Color) -> String {
    format!("{service_name}-{color}.container")
}

/// Env-var name carrying one color slot's host port: `SERVICE_PORT_HTTP_BLUE`
/// from the base `SERVICE_PORT_HTTP`. The two slots can't share a host port
/// (only one process binds it), so a blue/green install allocates a pair and
/// the renders below reference the color-specific one.
pub fn color_port_var(base_port_var: &str, color: Color) -> String {
    format!("{base_port_var}_{}", color.as_str().to_uppercase())
}

/// Rewrite a podman main-container quadlet into one color slot's variant:
/// rename the container and point every published host port at the
/// color-specific env var. The image, volumes, env file, and health command
/// are untouched — both slots are the same artifact, differing only in
/// identity and port. Aux quadlets (a bundled DB) are never colorized; only
/// the routable app container is.
pub fn podman_color_quadlet(content: &str, service_name: &str, color: Color) -> String {
    let mut out = String::with_capacity(content.len() + 16);
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("ContainerName=") {
            let indent = &line[..line.len() - trimmed.len()];
            // Only rewrite the app container's own name; defensively leave any
            // already-suffixed name alone so a re-render is idempotent.
            if rest.trim() == service_name {
                out.push_str(&format!("{indent}ContainerName={service_name}-{color}\n"));
                continue;
            }
        }
        out.push_str(&colorize_port_vars(line, color));
        out.push('\n');
    }
    out
}

/// Suffix every `${SERVICE_PORT_<NAME>}` reference on a line with the color
/// (`${SERVICE_PORT_HTTP}` -> `${SERVICE_PORT_HTTP_BLUE}`). Leaves the
/// container-port side of `PublishPort=host:container` and everything else
/// untouched, since only the host port comes from a `SERVICE_PORT_*` var.
fn colorize_port_vars(line: &str, color: Color) -> String {
    const MARKER: &str = "${SERVICE_PORT_";
    let suffix = format!("_{}", color.as_str().to_uppercase());
    let mut out = String::with_capacity(line.len() + suffix.len());
    let mut rest = line;
    while let Some(pos) = rest.find(MARKER) {
        let (before, from_marker) = rest.split_at(pos);
        out.push_str(before);
        // The var name runs from after the marker up to the closing `}`.
        match from_marker[MARKER.len()..].find('}') {
            Some(close_rel) => {
                let close = MARKER.len() + close_rel;
                out.push_str(&from_marker[..close]); // ${SERVICE_PORT_HTTP
                // Don't double-suffix if it's already colorized.
                if !from_marker[..close].ends_with(&suffix) {
                    out.push_str(&suffix);
                }
                out.push('}');
                rest = &from_marker[close + 1..];
            }
            // Malformed (no closing brace) — emit verbatim and stop scanning.
            None => {
                out.push_str(from_marker);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render one color slot's systemd unit for a native (non-container) service.
///
/// Shares the shape of the single-instance native unit but with two
/// differences that make blue/green work for *any* language: the process runs
/// from the color's own isolated working dir (`colors/<color>/` — a synced,
/// separately-built copy of the source, so an interpreted runtime's lazily-read
/// source files can't be mutated out from under the live slot), and an explicit
/// `Environment=` overrides the port so the two slots bind different ones. The
/// `Environment=` line comes *after* `EnvironmentFile=` so it wins over the
/// base `SERVICE_PORT_HTTP` from `.env`.
pub fn native_color_unit(p: &NativeColorUnit) -> String {
    format!(
        "[Unit]\n\
         Description={description} ({color})\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={workdir}\n\
         EnvironmentFile={home}/.env\n\
         Environment=SERVICE_HOME={home}\n\
         Environment={port_var}={port}\n\
         Environment=PATH=%h/.local/bin:%h/.cargo/bin:%h/.bun/bin:%h/.deno/bin:%h/go/bin:/usr/local/bin:/usr/bin:/bin\n\
         ExecStart=/bin/sh -c 'exec {run}'\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        description = p.description,
        color = p.color,
        workdir = p.workdir,
        home = p.home,
        port_var = p.port_var,
        port = p.port,
        run = p.run,
    )
}

/// Inputs to [`native_color_unit`]. `workdir` is the color's isolated slot
/// dir; `port` is the slot's allocated host port; `run` is the service's
/// `[service].run` command, executed unchanged in the slot dir.
pub struct NativeColorUnit<'a> {
    pub description: &'a str,
    pub color: Color,
    pub workdir: &'a str,
    pub home: &'a str,
    pub port_var: &'a str,
    pub port: u16,
    pub run: &'a str,
}

/// Everything [`color_swap_steps`] needs, assembled by the caller from the
/// per-runtime render. Keeping this a plain data struct (rather than threading
/// the registry/exposure through) makes the swap logic a pure function the
/// tests can pin without a live host.
pub struct ColorSwap {
    pub service_name: String,
    /// The slot serving traffic right now. The new version rolls onto
    /// `live.other()`.
    pub live: Color,
    /// Readies the idle slot's artifact: a [`Step::Build`] (native) or
    /// [`Step::PullImage`] (podman). `None` when the artifact is already in
    /// place and nothing needs (re)building.
    pub prepare: Option<Step>,
    /// Health probe against the *idle* slot's own port — ryra won't move
    /// traffic until this returns 200.
    pub health_url: String,
    pub health_timeout_secs: u32,
    /// The re-rendered Caddyfile ([`Step::WriteFile`]) with the upstream
    /// repointed at the idle color. `None` for a loopback install with no
    /// Caddy route, where the swap still works (the new slot simply takes over
    /// once the old one stops) but there's nothing to repoint.
    pub caddy_rewrite: Option<Step>,
}

impl ColorSwap {
    /// The slot the new version rolls onto — and the value the caller should
    /// persist as the install's new `active_color` once the plan succeeds.
    pub fn target(&self) -> Color {
        self.live.other()
    }
}

/// Build the ordered step list for a zero-downtime color swap.
///
/// The order is load-bearing: prepare and start the idle slot, *then* gate on
/// its health, *then* swap Caddy, and only then stop the old slot. If the
/// health gate times out the plan aborts before the Caddy swap, so the old slot
/// is still live and still routed — a failed deploy is a no-op, not an outage.
pub fn color_swap_steps(swap: ColorSwap) -> Vec<Step> {
    let target = swap.target();
    let start_unit = color_unit(&swap.service_name, target);
    let stop_unit = color_unit(&swap.service_name, swap.live);

    let mut steps = Vec::new();
    if let Some(prepare) = swap.prepare {
        steps.push(prepare);
    }
    steps.push(Step::StartService { unit: start_unit });
    steps.push(Step::WaitForHttpHealthy {
        url: swap.health_url,
        expect_status: 200,
        timeout_secs: swap.health_timeout_secs,
    });
    // Atomic cutover: rewrite the upstream, then reload Caddy (graceful — it
    // drains in-flight requests on the old upstream rather than dropping them).
    if let Some(rewrite) = swap.caddy_rewrite {
        steps.push(rewrite);
        steps.push(Step::ReloadCaddy);
    }
    steps.push(Step::StopService { unit: stop_unit });
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GeneratedFile;
    use std::path::PathBuf;

    fn caddy_write() -> Step {
        Step::WriteFile(GeneratedFile {
            path: PathBuf::from("/etc/caddy/Caddyfile"),
            content: "reverse_proxy app-green:8080".into(),
        })
    }

    #[test]
    fn target_is_the_other_color() {
        let swap = ColorSwap {
            service_name: "app".into(),
            live: Color::Blue,
            prepare: None,
            health_url: "http://127.0.0.1:9001/healthz".into(),
            health_timeout_secs: 60,
            caddy_rewrite: None,
        };
        assert_eq!(swap.target(), Color::Green);
    }

    #[test]
    fn podman_swap_has_canonical_order() {
        let steps = color_swap_steps(ColorSwap {
            service_name: "app".into(),
            live: Color::Green,
            prepare: Some(Step::PullImage {
                image: "ghcr.io/me/app:v2".into(),
            }),
            health_url: "http://127.0.0.1:9001/healthz".into(),
            health_timeout_secs: 60,
            caddy_rewrite: Some(caddy_write()),
        });
        // prepare -> start idle (blue) -> health -> caddy write -> reload -> stop old (green)
        assert!(matches!(steps[0], Step::PullImage { .. }));
        assert!(matches!(&steps[1], Step::StartService { unit } if unit == "app-blue"));
        assert!(matches!(steps[2], Step::WaitForHttpHealthy { .. }));
        assert!(matches!(steps[3], Step::WriteFile(_)));
        assert!(matches!(steps[4], Step::ReloadCaddy));
        assert!(matches!(&steps[5], Step::StopService { unit } if unit == "app-green"));
        assert_eq!(steps.len(), 6);
    }

    #[test]
    fn native_swap_builds_the_idle_slot_first() {
        let steps = color_swap_steps(ColorSwap {
            service_name: "api".into(),
            live: Color::Blue,
            prepare: Some(Step::Build {
                dir: PathBuf::from("/srv/api/colors/green"),
                command: "cargo build --release".into(),
            }),
            health_url: "http://127.0.0.1:9002/healthz".into(),
            health_timeout_secs: 120,
            caddy_rewrite: Some(caddy_write()),
        });
        // The build runs in the *idle* (green) slot's dir, never touching the
        // live (blue) slot still serving — language-agnostic isolation.
        match &steps[0] {
            Step::Build { dir, .. } => assert!(dir.ends_with("colors/green")),
            _ => panic!("expected Build step first"),
        }
        assert!(matches!(&steps[1], Step::StartService { unit } if unit == "api-green"));
        assert!(matches!(&steps[5], Step::StopService { unit } if unit == "api-blue"));
    }

    // --- render transforms ---------------------------------------------

    const AUTHELIA_QUADLET: &str = "\
[Container]
Image=docker.io/authelia/authelia:4.39
ContainerName=authelia
Network=authelia.network
PublishPort=${SERVICE_PORT_HTTP}:9091
Volume=${SERVICE_HOME}/config:/config:Z
EnvironmentFile=%h/.local/share/services/authelia/.env
";

    #[test]
    fn podman_quadlet_renames_container_and_colorizes_port() {
        let blue = podman_color_quadlet(AUTHELIA_QUADLET, "authelia", Color::Blue);
        assert!(blue.contains("ContainerName=authelia-blue"));
        assert!(!blue.contains("ContainerName=authelia\n"));
        assert!(blue.contains("PublishPort=${SERVICE_PORT_HTTP_BLUE}:9091"));
        // Image, network, volume, env file untouched — same artifact.
        assert!(blue.contains("Image=docker.io/authelia/authelia:4.39"));
        assert!(blue.contains("Network=authelia.network"));
        assert!(blue.contains("services/authelia/.env"));

        let green = podman_color_quadlet(AUTHELIA_QUADLET, "authelia", Color::Green);
        assert!(green.contains("ContainerName=authelia-green"));
        assert!(green.contains("PublishPort=${SERVICE_PORT_HTTP_GREEN}:9091"));
    }

    #[test]
    fn podman_quadlet_render_is_idempotent() {
        // Re-rendering an already-colorized quadlet must not double-suffix.
        let once = podman_color_quadlet(AUTHELIA_QUADLET, "authelia", Color::Blue);
        let twice = podman_color_quadlet(&once, "authelia", Color::Blue);
        assert_eq!(once, twice);
    }

    #[test]
    fn color_port_var_appends_uppercased_color() {
        assert_eq!(color_port_var("SERVICE_PORT_HTTP", Color::Blue), "SERVICE_PORT_HTTP_BLUE");
        assert_eq!(color_port_var("SERVICE_PORT_HTTP", Color::Green), "SERVICE_PORT_HTTP_GREEN");
    }

    #[test]
    fn native_color_unit_isolates_workdir_and_overrides_port() {
        let unit = native_color_unit(&NativeColorUnit {
            description: "Demo API",
            color: Color::Green,
            workdir: "/home/u/.local/share/services/api/colors/green",
            home: "/home/u/.local/share/services/api",
            port_var: "SERVICE_PORT_HTTP",
            port: 9002,
            run: "python -m app",
        });
        assert!(unit.contains("WorkingDirectory=/home/u/.local/share/services/api/colors/green"));
        // The port override must come AFTER EnvironmentFile so it wins.
        let envfile = unit.find("EnvironmentFile=").unwrap();
        let port_override = unit.find("Environment=SERVICE_PORT_HTTP=9002").unwrap();
        assert!(port_override > envfile, "port override must follow EnvironmentFile");
        assert!(unit.contains("ExecStart=/bin/sh -c 'exec python -m app'"));
        assert!(unit.contains("Description=Demo API (green)"));
    }

    // --- plan + render consistency, across several service shapes -------

    /// The unit names the swap plan starts/stops MUST match the unit names the
    /// renders produce, or a deploy would start a slot that doesn't exist.
    /// This pins that contract for a podman service end to end.
    #[test]
    fn e2e_podman_service_plan_matches_rendered_slots() {
        let svc = "authelia";
        let live = Color::Blue;

        // Render both slots the way the install path would.
        let blue_file = color_quadlet_filename(svc, Color::Blue);
        let green_file = color_quadlet_filename(svc, Color::Green);
        assert_eq!(blue_file, "authelia-blue.container");
        assert_eq!(green_file, "authelia-green.container");

        // Build the deploy plan (live=blue, so it rolls onto green).
        let swap = ColorSwap {
            service_name: svc.into(),
            live,
            prepare: Some(Step::PullImage { image: "authelia:4.40".into() }),
            health_url: "http://127.0.0.1:9002/api/health".into(),
            health_timeout_secs: 60,
            caddy_rewrite: Some(caddy_write()),
        };
        let target = swap.target();
        let steps = color_swap_steps(swap);

        // The unit started is the green slot's unit, whose quadlet file we render.
        let started = match &steps[1] {
            Step::StartService { unit } => unit.clone(),
            _ => panic!("expected StartService at index 1"),
        };
        assert_eq!(started, color_unit(svc, target));
        assert_eq!(format!("{started}.container"), green_file);
    }

    /// Same contract for a native (here: Python) service — proving the swap
    /// choreography is runtime-agnostic.
    #[test]
    fn e2e_native_python_service_plan_matches_rendered_slots() {
        let svc = "api";
        let green_unit = native_color_unit(&NativeColorUnit {
            description: "API",
            color: Color::Green,
            workdir: "/srv/api/colors/green",
            home: "/srv/api",
            port_var: "SERVICE_PORT_HTTP",
            port: 9002,
            run: "python -m app",
        });
        // The rendered green slot runs on 9002; the plan's health probe must
        // hit that same port.
        assert!(green_unit.contains("Environment=SERVICE_PORT_HTTP=9002"));

        let steps = color_swap_steps(ColorSwap {
            service_name: svc.into(),
            live: Color::Blue,
            prepare: Some(Step::Build {
                dir: "/srv/api/colors/green".into(),
                command: "pip install -r requirements.txt".into(),
            }),
            health_url: "http://127.0.0.1:9002/healthz".into(),
            health_timeout_secs: 90,
            caddy_rewrite: None,
        });
        assert!(matches!(&steps[1], Step::StartService { unit } if unit == "api-green"));
        match &steps[0] {
            Step::Build { dir, .. } => assert_eq!(dir.to_str().unwrap(), "/srv/api/colors/green"),
            _ => panic!("expected Build in the green slot dir"),
        }
    }

    #[test]
    fn loopback_install_skips_caddy_but_still_swaps() {
        let steps = color_swap_steps(ColorSwap {
            service_name: "app".into(),
            live: Color::Blue,
            prepare: None,
            health_url: "http://127.0.0.1:9002/healthz".into(),
            health_timeout_secs: 30,
            caddy_rewrite: None,
        });
        // No prepare, no caddy: start idle -> health -> stop old.
        assert!(matches!(&steps[0], Step::StartService { unit } if unit == "app-green"));
        assert!(matches!(steps[1], Step::WaitForHttpHealthy { .. }));
        assert!(matches!(&steps[2], Step::StopService { unit } if unit == "app-blue"));
        assert!(!steps.iter().any(|s| matches!(s, Step::ReloadCaddy)));
        assert_eq!(steps.len(), 3);
    }
}
