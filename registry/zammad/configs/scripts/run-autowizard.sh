#!/bin/bash
# Run Zammad's first-run setup wizard non-interactively. The entrypoint
# drops the auto_wizard.json payload into place from $AUTOWIZARD_JSON
# (base64-decoded), but doesn't execute it — that normally happens on
# the first web wizard visit. We trigger it directly via rails runner.
#
# Idempotent: we skip when an Admin user already exists, and AutoWizard
# itself is a no-op when the payload file is missing. ExecStartPost scripts
# must never return non-zero (would block the service from starting).
set -u

echo "autowizard: waiting for railsserver to respond on :3000 (up to 120 × 2s)"
for i in {1..120}; do
  if podman exec zammad-railsserver curl -sS -o /dev/null -m 3 "http://localhost:3000/" 2>/dev/null; then
    echo "autowizard: railsserver responded after ${i} attempt(s)"
    break
  fi
  # Emit a heartbeat every 5 attempts so the journal (and the CLI spinner
  # that tails it) shows we haven't stalled.
  if [ $((i % 5)) -eq 0 ]; then
    echo "autowizard: still waiting for railsserver (attempt ${i}/120)"
  fi
  sleep 2
done

podman exec zammad-railsserver bundle exec rails r '
  admin_exists = User.joins(:roles).where(roles: { name: "Admin" }).exists?
  if admin_exists
    puts "autowizard: admin already exists, skipping setup"
  elsif !AutoWizard.enabled?
    puts "autowizard: no payload file — skipping"
  else
    u = AutoWizard.setup
    puts "autowizard: setup complete, admin=#{u&.email.inspect}"
  end
  # CheckSetup.done? has a self-healing side effect: when an admin exists
  # but system_init_done is false, it flips the flag. Triggering it here
  # avoids a fresh browser from being redirected back to #getting_started.
  Service::System::CheckSetup.done?
  puts "autowizard: system_init_done=#{Setting.get("system_init_done")}"
' 2>&1 || echo "run-autowizard.sh: rails runner exited non-zero — inspect logs; user may need to complete wizard manually at $ZAMMAD_HTTP_TYPE://$ZAMMAD_FQDN" >&2
exit 0
