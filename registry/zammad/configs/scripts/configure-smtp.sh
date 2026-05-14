#!/bin/bash
# Configure Zammad's outbound mail channel via rails runner. Zammad's
# Channel model stores SMTP config as a hash on a Channel row with
# area="Email::Notification"; setting it here mirrors what the admin UI does.
#
# ExecStartPost scripts must never return non-zero (it kills the service).
# We log failures and exit 0.
set -u
[ -z "${SMTP_HOST:-}" ] && exit 0

echo "smtp: waiting for railsserver on :3000 (up to 120 × 2s)"
for i in {1..120}; do
  if podman exec zammad-railsserver curl -sS -o /dev/null -m 3 "http://localhost:3000/" 2>/dev/null; then
    echo "smtp: railsserver ready after ${i} attempt(s)"
    break
  fi
  if [ $((i % 5)) -eq 0 ]; then
    echo "smtp: still waiting for railsserver (attempt ${i}/120)"
  fi
  sleep 2
done

if ! podman exec \
  -e SMTP_HOST="$SMTP_HOST" \
  -e SMTP_PORT="$SMTP_PORT" \
  -e SMTP_USER="${SMTP_USER:-}" \
  -e SMTP_PASS="${SMTP_PASS:-}" \
  -e SMTP_FROM="$SMTP_FROM" \
  -e SMTP_SECURITY="${SMTP_SECURITY:-none}" \
  zammad-railsserver bundle exec rails r '
    ssl_mode = case ENV["SMTP_SECURITY"]
              when "ssl", "tls"    then "ssl"
              when "starttls"      then "starttls"
              else                      "off"
              end

    outbound = {
      adapter: "smtp",
      options: {
        host:           ENV["SMTP_HOST"],
        port:           ENV["SMTP_PORT"].to_i,
        user:           ENV["SMTP_USER"],
        password:       ENV["SMTP_PASS"],
        ssl_verify:     false,
        enable_starttls_auto: ssl_mode == "starttls",
        ssl:            ssl_mode == "ssl",
      },
    }

    channel = Channel.find_by(area: "Email::Notification") || Channel.new(area: "Email::Notification")
    channel.options = { outbound: outbound, inbound: { adapter: "null", options: {} } }
    channel.group_id = Group.first&.id
    channel.active = true
    channel.save!

    Setting.set("notification_sender", ENV["SMTP_FROM"])
    puts "Zammad SMTP configured"
  ' 2>&1
then
  echo "configure-smtp.sh: failed to configure SMTP — re-run manually after first admin is created" >&2
fi
exit 0
