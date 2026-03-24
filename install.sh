#!/bin/sh
set -eu

REPO="ryanravn/ryra"
BASE_URL="https://github.com/${REPO}/releases/download/latest"
APT_URL="https://ryanravn.github.io/ryra"

main() {
    arch=$(uname -m)
    case "$arch" in
        x86_64)  rust_target="x86_64-unknown-linux-gnu"; deb_arch="amd64" ;;
        aarch64) rust_target="aarch64-unknown-linux-gnu"; deb_arch="arm64" ;;
        *)
            echo "Error: unsupported architecture: $arch"
            exit 1
            ;;
    esac

    if command -v apt-get >/dev/null 2>&1; then
        install_apt
    elif command -v dnf >/dev/null 2>&1; then
        install_rpm
    elif command -v pacman >/dev/null 2>&1; then
        install_pacman
    else
        install_binary
    fi

    echo ""
    echo "ryra installed successfully! Run 'ryra init' to get started."
}

install_apt() {
    echo "Detected Debian/Ubuntu — setting up APT repository..."

    sudo mkdir -p /etc/apt/keyrings
    curl -fsSL "${APT_URL}/gpg.key" | sudo gpg --dearmor -o /etc/apt/keyrings/ryra.gpg
    echo "deb [arch=${deb_arch} signed-by=/etc/apt/keyrings/ryra.gpg] ${APT_URL} stable main" \
        | sudo tee /etc/apt/sources.list.d/ryra.list > /dev/null

    sudo apt-get update -o Dir::Etc::sourcelist="/etc/apt/sources.list.d/ryra.list" \
        -o Dir::Etc::sourceparts="-" -o APT::Get::List-Cleanup="0"
    sudo apt-get install -y ryra

    echo "Future updates will be included in 'sudo apt update && sudo apt upgrade'."
}

install_rpm() {
    echo "Detected Fedora/RHEL — COPR package coming soon."
    echo "Installing binary directly for now..."
    install_binary
}

install_pacman() {
    echo "Detected Arch Linux — AUR package coming soon."
    echo "Installing binary directly for now..."
    install_binary
}

install_binary() {
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    url="${BASE_URL}/ryra-${rust_target}.tar.gz"
    echo "Downloading ryra for ${arch}..."
    curl -fsSL -o "${tmp}/ryra.tar.gz" "$url"

    tar xzf "${tmp}/ryra.tar.gz" -C "${tmp}"
    sudo install -m 755 "${tmp}/ryra" /usr/local/bin/ryra

    echo "Installed to /usr/local/bin/ryra"
    echo "To update, re-run this script."
}

main
