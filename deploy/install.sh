#!/bin/sh
# Installs VanityGözcü as a systemd service on a Linux host. Run as root from a
# checkout that has a release build:
#   cargo build --release && sudo sh deploy/install.sh
# Optional argument: path to the binary (default: target/release/vanity-gozcu).
# It never overwrites an existing config or env file and does not start the service.
set -eu

APP_USER=vanity
APP_DIR=/opt/vanity-gozcu
ENV_DIR=/etc/vanity-gozcu
ENV_FILE="$ENV_DIR/env"
UNIT=/etc/systemd/system/vanity-gozcu.service

script_dir=$(cd "$(dirname "$0")" && pwd)
repo_dir=$(dirname "$script_dir")
binary=${1:-"$repo_dir/target/release/vanity-gozcu"}

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root (sudo sh deploy/install.sh)" >&2
    exit 1
fi
if [ ! -x "$binary" ]; then
    echo "error: binary not found at $binary (run 'cargo build --release' first)" >&2
    exit 1
fi
for f in "$repo_dir/config.example.toml" "$script_dir/vanity-gozcu.service" "$script_dir/env.example"; do
    if [ ! -f "$f" ]; then
        echo "error: missing $f" >&2
        exit 1
    fi
done

if ! id "$APP_USER" >/dev/null 2>&1; then
    nologin=$(command -v nologin || echo /usr/sbin/nologin)
    useradd --system --home-dir "$APP_DIR" --no-create-home --shell "$nologin" "$APP_USER"
    echo "created system user $APP_USER"
fi

install -d -o root -g root -m 755 "$APP_DIR"
install -d -o "$APP_USER" -g "$APP_USER" -m 750 "$APP_DIR/data" "$APP_DIR/logs"
install -o root -g root -m 755 "$binary" "$APP_DIR/vanity-gozcu"

config="$APP_DIR/data/config.toml"
if [ -e "$config" ]; then
    echo "keeping existing $config"
else
    install -o "$APP_USER" -g "$APP_USER" -m 640 "$repo_dir/config.example.toml" "$config"
    echo "wrote example config to $config"
fi

install -d -o root -g "$APP_USER" -m 750 "$ENV_DIR"
if [ -e "$ENV_FILE" ]; then
    echo "keeping existing $ENV_FILE"
else
    install -o "$APP_USER" -g "$APP_USER" -m 600 "$script_dir/env.example" "$ENV_FILE"
    echo "wrote empty secrets template to $ENV_FILE"
fi

install -o root -g root -m 644 "$script_dir/vanity-gozcu.service" "$UNIT"
systemctl daemon-reload

cat <<MSG

Installed. Next steps:
  1. Edit $config (targets, notifiers).
  2. Put secrets into $ENV_FILE (VG_DISCORD_WEBHOOK, VG_TELEGRAM_TOKEN, ...).
  3. systemctl enable --now vanity-gozcu
  4. Watch it live (the socket path is relative to the working directory):
       cd $APP_DIR && sudo -u $APP_USER env VG_CONFIG=$config ./vanity-gozcu attach
  Logs: journalctl -u vanity-gozcu -f   and   $APP_DIR/logs/
MSG
