#!/bin/sh
set -eu

REPO="ryanravn/ryra"
APT_URL="https://ryanravn.github.io/ryra"
KEYRING="/etc/apt/keyrings/ryra.gpg"
SOURCES="/etc/apt/sources.list.d/ryra.list"

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

    echo "Adding ryra APT repository..."

    sudo mkdir -p /etc/apt/keyrings
    curl -fsSL "${APT_URL}/gpg.key" | sudo gpg --dearmor -o "$KEYRING"
    echo "deb [arch=${deb_arch} signed-by=${KEYRING}] ${APT_URL} stable main" | sudo tee "$SOURCES" > /dev/null

    echo "Installing ryra..."
    sudo apt-get update -o Dir::Etc::sourcelist="$SOURCES" -o Dir::Etc::sourceparts="-" -o APT::Get::List-Cleanup="0"
    sudo apt-get install -y ryra

    echo "ryra installed successfully! Run 'ryra init' to get started."
    echo "Future updates will be included in 'sudo apt update && sudo apt upgrade'."
}

main
