#!/bin/bash
# Configure Nextcloud's outbound mail via `occ config:system:set`.
#
# We deliberately DON'T use Nextcloud's first-install SMTP env vars
# (SMTP_HOST, MAIL_FROM_ADDRESS, MAIL_DOMAIN) — those only apply at the
# very first boot, so re-running `ryra add --smtp=<provider>` after the
# initial install would silently do nothing. `occ` is idempotent.
#
# Guard: skip cleanly when ryra wasn't invoked with --smtp.
set -euo pipefail
[ -z "${SMTP_HOST:-}" ] && exit 0

# Map ryra's SMTP security value to Nextcloud's mail_smtpsecure setting.
# Nextcloud: "" = plain, "tls" = STARTTLS, "ssl" = implicit TLS (SMTPS).
case "${SMTP_SECURITY:-off}" in
  starttls)  SMTPSECURE="tls" ;;
  force_tls) SMTPSECURE="ssl" ;;
  *)         SMTPSECURE="" ;;
esac

# Split smtp.from into local-part and domain for mail_from_address / mail_domain.
FROM_LOCAL="${SMTP_FROM%@*}"
FROM_DOMAIN="${SMTP_FROM#*@}"

echo "Waiting for Nextcloud to finish installing..."
for i in $(seq 1 120); do
  STATUS=$(podman exec -u www-data nextcloud php occ status --output=json 2>/dev/null || true)
  echo "$STATUS" | grep -q '"installed":true' && break
  sleep 5
done

set_config() {
  podman exec -u www-data nextcloud php occ config:system:set "$1" --value="$2" 2>&1 \
    || echo "config:system:set $1 failed (non-fatal)"
}

set_config mail_smtpmode      "smtp"
set_config mail_sendmailmode  "smtp"
set_config mail_smtphost      "$SMTP_HOST"
set_config mail_smtpport      "$SMTP_PORT"
set_config mail_smtpsecure    "$SMTPSECURE"
set_config mail_smtpauthtype  "LOGIN"
if [ -n "${SMTP_USERNAME:-}" ]; then
  set_config mail_smtpauth    "1"
  set_config mail_smtpname    "$SMTP_USERNAME"
  set_config mail_smtppassword "$SMTP_PASSWORD"
else
  set_config mail_smtpauth    "0"
fi
set_config mail_from_address  "$FROM_LOCAL"
set_config mail_domain        "$FROM_DOMAIN"

echo "Nextcloud SMTP configured: $SMTP_HOST:$SMTP_PORT ($SMTPSECURE)"
