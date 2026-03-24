---
title: CLI Commands
description: Complete reference for all Ryra CLI commands.
---

## `ryra init`

Initialize Ryra on a host. Installs system dependencies (podman, nginx, systemd-container) and sets up the base configuration. Optional — `ryra add` will run init automatically if needed.

```bash
ryra init
```

## `ryra add`

Add and start a service.

```bash
ryra add <service>
```

Creates the service user, pulls the container image, generates quadlet units, configures nginx, and starts the service. Prompts for exposure mode and any required configuration.

## `ryra remove`

Remove a service completely.

```bash
ryra remove <service>
```

Stops the container, removes systemd units, nginx config, and the service user.

## `ryra update`

Re-scaffold a service with the latest registry definition.

```bash
ryra update <service>
```

:::caution
This is a destructive operation. It recreates the quadlet and nginx config from scratch. Use `ryra diff` first to review changes.
:::

## `ryra diff`

Show what changed in a service's registry definition since it was installed.

```bash
ryra diff <service>
```

Compares the installed snapshot against the current registry definition.

## `ryra expose`

Change how a service is exposed.

```bash
ryra expose <service>
```

See [Exposure Modes](/guides/exposure-modes/) for details on each mode.

## `ryra config`

View or edit global configuration.

```bash
# Interactive config menu
ryra config

# Configure a specific section
ryra config dns
ryra config ssl
ryra config smtp
ryra config repo
```

## `ryra status`

Show global config or details about a specific service.

```bash
# Global status
ryra status

# Service-specific
ryra status <service>
```

## `ryra list`

List all installed services.

```bash
ryra list
```

## `ryra search`

Browse available services in the configured registry.

```bash
ryra search
```

## `ryra reset`

Tear down all services, containers, and configuration.

```bash
ryra reset
```

:::danger
This removes everything Ryra has set up — all service users, containers, quadlets, nginx configs, and the global config. Use with care.
:::

## `ryra test`

Run tests for services. Can test against running services or spin up ephemeral QEMU VMs.

```bash
# Test a specific service in a VM
ryra test --vm <service>

# Test with a specific distro
ryra test --vm --distro=fedora-43 <service>

# Run all tests
ryra test --vm
```
