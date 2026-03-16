/// Parameters for generating an nginx site configuration.
pub struct NginxSiteParams<'a> {
    pub service_name: &'a str,
    pub domain: &'a str,
    pub upstream_port: u16,
}

/// Render an nginx reverse-proxy site config.
pub fn render_site(params: &NginxSiteParams) -> String {
    format!(
        r#"# Managed by ryra — do not edit manually
upstream {name} {{
    server 127.0.0.1:{port};
}}

server {{
    listen 80;
    server_name {domain};

    location / {{
        proxy_pass http://{name};
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        # WebSocket support
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }}
}}
"#,
        name = params.service_name,
        domain = params.domain,
        port = params.upstream_port,
    )
}

/// Render the main ryra nginx container quadlet (root-level).
pub fn render_nginx_quadlet() -> String {
    r#"[Unit]
Description=Ryra nginx reverse proxy

[Container]
Image=docker.io/library/nginx:alpine
PublishPort=80:80
PublishPort=443:443
Volume=/etc/ryra/nginx/nginx.conf:/etc/nginx/nginx.conf:ro
Volume=/etc/ryra/nginx/sites:/etc/nginx/conf.d:ro

[Service]
Restart=always
TimeoutStartSec=60

[Install]
WantedBy=multi-user.target
"#
    .to_string()
}

/// Render the base nginx.conf that includes sites.
pub fn render_nginx_base_conf() -> String {
    r#"worker_processes auto;
error_log /var/log/nginx/error.log warn;
pid /tmp/nginx.pid;

events {
    worker_connections 1024;
}

http {
    include /etc/nginx/mime.types;
    default_type application/octet-stream;

    log_format main '$remote_addr - $remote_user [$time_local] "$request" '
                    '$status $body_bytes_sent "$http_referer" '
                    '"$http_user_agent" "$http_x_forwarded_for"';

    access_log /var/log/nginx/access.log main;
    sendfile on;
    keepalive_timeout 65;
    client_max_body_size 100m;

    include /etc/nginx/conf.d/*.conf;
}
"#
    .to_string()
}
