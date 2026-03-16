/// Render the cloudflared tunnel quadlet (root-level, alongside nginx).
pub fn render_cloudflared_quadlet(tunnel_token: &str) -> String {
    format!(
        r#"[Unit]
Description=Cloudflare Tunnel
After=nginx.service

[Container]
Image=docker.io/cloudflare/cloudflared:latest
Exec=tunnel --no-autoupdate run --token {token}
Network=host

[Service]
Restart=always
TimeoutStartSec=60

[Install]
WantedBy=multi-user.target
"#,
        token = tunnel_token,
    )
}
