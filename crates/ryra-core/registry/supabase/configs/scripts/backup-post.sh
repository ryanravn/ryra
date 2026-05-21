#!/bin/bash
# Bring the stack back. Starting `supabase.service` cascades to
# the rest via Requires=. supavisor is Wants= and tags along.
set -euo pipefail
systemctl --user start supabase.service
