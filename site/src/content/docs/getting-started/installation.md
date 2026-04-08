---
title: Installation
description: How to install Ryra on your server.
prev:
  label: Introduction
  link: /
---

## Quick install

The install script detects your distro and sets up the appropriate package repository:

```bash
curl -fsSL https://raw.githubusercontent.com/ryanravn/ryra/main/install.sh | sh
```

This adds the Ryra repo for your package manager and installs the `ryra` binary. Updates come through your normal system updates (`apt upgrade`, `dnf upgrade`, `pacman -Syu`).

## Supported distributions

| Distro | Package format | Package manager |
|--------|---------------|-----------------|
| Debian 13+ / Ubuntu 24.04+ | `.deb` | APT |
| Fedora 43+ | `.rpm` | DNF |
| Arch Linux | `.pkg.tar.zst` | Pacman |

## Prerequisites

Ryra installs its own dependencies (`podman`, `systemd-container`, etc.) when you run `ryra init` or `ryra add`. Your server needs:

- A supported Linux distribution (see above)
- `sudo` access
- An internet connection (to pull container images)

Ryra is Linux-only. It requires systemd and rootless Podman, which are not available on other platforms.

## Verify installation

```bash
ryra --version
```
