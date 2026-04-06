#!/bin/sh
set -eu

REPO="ryanravn/ryra"
PAGES_URL="https://ryanravn.github.io/ryra"
BASE_URL="https://github.com/${REPO}/releases/download/latest"

main() {
    os=$(uname -s)
    arch=$(uname -m)

    if [ "$os" != "Linux" ]; then
        echo "Error: ryra requires Linux and is not supported on ${os}."
        exit 1
    fi

    case "${arch}" in
        x86_64)   rust_target="x86_64-unknown-linux-gnu" ;;
        aarch64)  rust_target="aarch64-unknown-linux-gnu" ;;
        *)
            echo "Error: unsupported architecture: ${arch}"
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
    curl -fsSL "${PAGES_URL}/gpg.key" | sudo gpg --dearmor -o /etc/apt/keyrings/ryra.gpg

    deb_arch=$(dpkg --print-architecture)
    echo "deb [arch=${deb_arch} signed-by=/etc/apt/keyrings/ryra.gpg] ${PAGES_URL}/deb stable main" \
        | sudo tee /etc/apt/sources.list.d/ryra.list > /dev/null

    sudo apt-get update -o Dir::Etc::sourcelist="/etc/apt/sources.list.d/ryra.list" \
        -o Dir::Etc::sourceparts="-" -o APT::Get::List-Cleanup="0"
    sudo apt-get install -y ryra

    echo "Future updates: sudo apt update && sudo apt upgrade"
}

install_rpm() {
    echo "Detected Fedora/RHEL — setting up RPM repository..."

    sudo rpm --import "${PAGES_URL}/gpg.key"

    rpm_arch=$(uname -m)
    cat <<EOF | sudo tee /etc/yum.repos.d/ryra.repo > /dev/null
[ryra]
name=ryra
baseurl=${PAGES_URL}/rpm/${rpm_arch}
enabled=1
gpgcheck=1
gpgkey=${PAGES_URL}/gpg.key
EOF

    sudo dnf install -y ryra

    echo "Future updates: sudo dnf upgrade ryra"
}

install_pacman() {
    echo "Detected Arch Linux — setting up Pacman repository..."

    sudo pacman-key --init
    curl -fsSL "${PAGES_URL}/gpg.key" | sudo pacman-key --add -
    KEY_ID=$(curl -fsSL "${PAGES_URL}/gpg.key" | gpg --with-colons --import-options show-only --import 2>/dev/null | awk -F: '/^pub/{print $5}')
    sudo pacman-key --lsign-key "$KEY_ID"

    pac_arch=$(uname -m)
    if ! grep -q '\[ryra\]' /etc/pacman.conf; then
        cat <<EOF | sudo tee -a /etc/pacman.conf > /dev/null

[ryra]
SigLevel = Required
Server = ${PAGES_URL}/pacman/${pac_arch}
EOF
    fi

    sudo pacman -Sy --noconfirm ryra

    echo "Future updates: sudo pacman -Syu"
}

install_binary() {
    echo "Installing binary directly..."

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
