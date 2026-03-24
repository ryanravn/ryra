#!/bin/sh
set -eu

REPO="ryanravn/ryra"
BASE_URL="https://github.com/${REPO}/releases/download/latest"

main() {
    if ! command -v apt-get >/dev/null 2>&1; then
        echo "Error: ryra currently only supports Debian-based systems (Debian, Ubuntu, etc.)"
        exit 1
    fi

    arch=$(uname -m)
    case "$arch" in
        x86_64)  deb_arch="amd64" ;;
        aarch64) deb_arch="arm64" ;;
        *)
            echo "Error: unsupported architecture: $arch"
            exit 1
            ;;
    esac

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    url="${BASE_URL}/ryra_${deb_arch}.deb"
    echo "Downloading ryra for ${deb_arch}..."
    curl -fsSL -o "${tmp}/ryra.deb" "$url"

    echo "Installing..."
    sudo dpkg -i "${tmp}/ryra.deb"

    echo "ryra installed successfully! Run 'ryra init' to get started."
}

main
