#!/bin/bash
# Wait for ClickHouse and Postgres, run migrations, start PostHog web server.
set -e

echo "Waiting for Postgres..."
until python -c "
import socket
s = socket.socket()
s.settimeout(5)
s.connect(('db', 5432))
s.close()
" 2>/dev/null; do
    sleep 1
done
echo "Postgres ready"

echo "Waiting for ClickHouse..."
until python -c "
import socket
s = socket.socket()
s.settimeout(5)
s.connect(('clickhouse', 8123))
s.close()
" 2>/dev/null; do
    sleep 1
done
echo "ClickHouse ready"

echo "Running migrations..."
python manage.py migrate --noinput
python manage.py migrate_clickhouse --noinput

echo "Starting PostHog..."
exec gunicorn posthog.wsgi --config gunicorn.config.py --bind 0.0.0.0:8000 --log-level info
