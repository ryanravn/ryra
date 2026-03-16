use std::path::PathBuf;

/// How nginx should handle SSL for this site.
pub enum SiteMode {
    /// Tunnel handles SSL — nginx just serves HTTP on localhost.
    Tunnel,
    /// Direct exposure — nginx terminates SSL with certs.
    Ssl {
        cert_path: PathBuf,
        key_path: PathBuf,
    },
}

/// Parameters for generating an nginx site configuration.
pub struct NginxSiteParams<'a> {
    pub service_name: &'a str,
    pub domain: &'a str,
    pub upstream_port: u16,
    pub mode: SiteMode,
}

/// Render an nginx reverse-proxy site config.
pub fn render_site(params: &NginxSiteParams) -> String {
    match &params.mode {
        SiteMode::Tunnel => render_http_site(params),
        SiteMode::Ssl { cert_path, key_path } => render_ssl_site(params, cert_path, key_path),
    }
}

fn render_http_site(params: &NginxSiteParams) -> String {
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

fn render_ssl_site(params: &NginxSiteParams, cert_path: &PathBuf, key_path: &PathBuf) -> String {
    format!(
        r#"# Managed by ryra — do not edit manually
upstream {name} {{
    server 127.0.0.1:{port};
}}

server {{
    listen 80;
    server_name {domain};
    return 301 https://$host$request_uri;
}}

server {{
    listen 443 ssl;
    server_name {domain};

    ssl_certificate {cert};
    ssl_certificate_key {key};
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_ciphers HIGH:!aNULL:!MD5;
    ssl_prefer_server_ciphers off;
    ssl_ecdh_curve X25519:prime256v1:secp384r1;

    location / {{
        proxy_pass http://{name};
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;

        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
    }}
}}
"#,
        name = params.service_name,
        domain = params.domain,
        port = params.upstream_port,
        cert = cert_path.display(),
        key = key_path.display(),
    )
}

/// Whether nginx should publish ports to the host or only listen locally.
pub enum NginxExposure {
    /// Publish 80/443 to the world (no tunnel).
    Public,
    /// Listen only on localhost (tunnel handles external traffic).
    LocalOnly,
}

/// Render the main ryra nginx container quadlet (root-level).
pub fn render_nginx_quadlet(exposure: &NginxExposure) -> String {
    let ports = match exposure {
        NginxExposure::Public => "\
PublishPort=80:80
PublishPort=443:443"
            .to_string(),
        NginxExposure::LocalOnly => "\
PublishPort=127.0.0.1:80:80"
            .to_string(),
    };

    // Only mount certs volume if exposing publicly (tunnel mode doesn't need certs)
    let cert_volume = match exposure {
        NginxExposure::Public => "\nVolume=/etc/ryra/certs:/etc/ryra/certs:ro",
        NginxExposure::LocalOnly => "",
    };

    format!(
        r#"[Unit]
Description=Ryra nginx reverse proxy

[Container]
Image=docker.io/library/nginx:alpine
{ports}
Volume=/etc/ryra/nginx/nginx.conf:/etc/nginx/nginx.conf:ro
Volume=/etc/ryra/nginx/sites:/etc/nginx/conf.d:ro{cert_volume}

[Service]
Restart=always
TimeoutStartSec=60

[Install]
WantedBy=multi-user.target
"#
    )
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
