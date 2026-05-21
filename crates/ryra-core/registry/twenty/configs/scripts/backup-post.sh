#!/bin/bash
set -euo pipefail
systemctl --user start twenty.service
systemctl --user start twenty-worker.service
