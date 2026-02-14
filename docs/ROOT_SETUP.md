# Forgejo Arch Setup (Root Steps)

This runbook assumes Arch package `forgejo` is installed.

## 1) Install hardened localhost config

```bash
sudo cp -a /etc/forgejo/app.ini "/etc/forgejo/app.ini.bak.$(date +%Y%m%d-%H%M%S)"
sudo install -m 0660 -o root -g forgejo /home/main/forgejo-agent/templates/app.ini /etc/forgejo/app.ini

SECRET_KEY="$(forgejo generate secret SECRET_KEY)"
INTERNAL_TOKEN="$(forgejo generate secret INTERNAL_TOKEN)"
JWT_SECRET="$(forgejo generate secret JWT_SECRET)"

sudo sed -i \
  -e "s|__SECRET_KEY__|$SECRET_KEY|g" \
  -e "s|__INTERNAL_TOKEN__|$INTERNAL_TOKEN|g" \
  -e "s|__JWT_SECRET__|$JWT_SECRET|g" \
  /etc/forgejo/app.ini
```

## 2) Start service + data-path sanity

```bash
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/forgejo.conf
sudo systemctl enable --now forgejo

# If logs mention /usr/bin/data/tmp/package-upload, inject APP_DATA_PATH into [server].
if ! sudo awk '
  BEGIN { in_server=0; found=0 }
  /^\[server\]/ { in_server=1; next }
  /^\[/ { in_server=0 }
  in_server && /^APP_DATA_PATH[[:space:]]*=/ { found=1 }
  END { exit(found ? 0 : 1) }
' /etc/forgejo/app.ini; then
  sudo sed -i '/^\[server\]/a APP_DATA_PATH = /var/lib/forgejo/data' /etc/forgejo/app.ini
fi

sudo systemctl restart forgejo
sudo systemctl status forgejo --no-pager
```

Expected: service `active (running)` and UI at `http://127.0.0.1:3000`.

## 3) Create admin user + token (as `forgejo` user)

`MIN_PASSWORD_LENGTH = 20` in this config.

```bash
ADMIN_PW='REPLACE_WITH_LONG_RANDOM_PASSWORD_AT_LEAST_20_CHARS'

sudo -u forgejo forgejo admin user create \
  --config /etc/forgejo/app.ini \
  --username main \
  --email main@localhost \
  --admin \
  --must-change-password=false \
  --password "$ADMIN_PW" || true

TOKEN="$(sudo -u forgejo forgejo admin user generate-access-token \
  --config /etc/forgejo/app.ini \
  --username main \
  --token-name "codex-main-$(date +%Y%m%d-%H%M%S)" \
  --scopes all \
  --raw)"

/home/main/forgejo-agent/bin/forgejo-agent-init "$TOKEN"
```

## 4) Install Rust gateway binary

```bash
/home/main/forgejo-agent/scripts/install.sh
```

Binary path: `/home/main/.local/bin/forgejoctl`.

## 5) Bootstrap queue + validate

```bash
/home/main/.local/bin/forgejoctl repo ensure main/backlog
/home/main/.local/bin/forgejoctl whoami
/home/main/.local/bin/forgejoctl issue list main/backlog --state open
```

## 6) Optional hardening check

```bash
sudo ss -ltnp | rg 'forgejo|:3000'
# Should show 127.0.0.1:3000, not 0.0.0.0:3000
```

## 7) Optional role users + isolated interactive sessions

Create local OS users for role separation:

```bash
id -u codex-dev >/dev/null 2>&1 || sudo useradd --create-home --shell /bin/bash codex-dev
id -u codex-orch >/dev/null 2>&1 || sudo useradd --create-home --shell /bin/bash codex-orch

sudo install -d -m 0755 -o codex-dev -g codex-dev /home/codex-dev/.config/forgejo-agent
sudo install -d -m 0755 -o codex-orch -g codex-orch /home/codex-orch/.config/forgejo-agent
```

Create Forgejo users and tokens (run admin CLI as `forgejo`):

```bash
DEV_PW="$(openssl rand -base64 24)"
ORCH_PW="$(openssl rand -base64 24)"

sudo -u forgejo forgejo admin user create \
  --config /etc/forgejo/app.ini \
  --username codex-dev \
  --email codex-dev@localhost \
  --must-change-password=false \
  --password "$DEV_PW" || true

sudo -u forgejo forgejo admin user create \
  --config /etc/forgejo/app.ini \
  --username codex-orch \
  --email codex-orch@localhost \
  --admin \
  --must-change-password=false \
  --password "$ORCH_PW" || true

DEV_TOKEN="$(sudo -u forgejo forgejo admin user generate-access-token \
  --config /etc/forgejo/app.ini \
  --username codex-dev \
  --token-name "codex-dev-$(date +%Y%m%d-%H%M%S)" \
  --scopes all \
  --raw)"

ORCH_TOKEN="$(sudo -u forgejo forgejo admin user generate-access-token \
  --config /etc/forgejo/app.ini \
  --username codex-orch \
  --token-name "codex-orch-$(date +%Y%m%d-%H%M%S)" \
  --scopes all \
  --raw)"
```

Store tokens as root-managed credentials:

```bash
sudo install -d -m 0750 -o root -g root /etc/forgejo-agent/creds
printf '%s\n' "$DEV_TOKEN" | sudo install -m 0640 -o root -g codex-dev /dev/stdin /etc/forgejo-agent/creds/codex-dev.token
printf '%s\n' "$ORCH_TOKEN" | sudo install -m 0640 -o root -g codex-orch /dev/stdin /etc/forgejo-agent/creds/codex-orch.token
```

Write minimal per-role config files (no token path needed):

```bash
sudo tee /home/codex-dev/.config/forgejo-agent/config.env >/dev/null <<'EOF'
FORGEJO_BASE_URL=http://127.0.0.1:3000
FORGEJO_DEFAULT_OWNER=main
FORGEJO_DEFAULT_REPO=backlog
FORGEJO_AGENT_NAME=codex-dev
FORGEJO_LEASE_MINUTES=90
EOF
sudo chown codex-dev:codex-dev /home/codex-dev/.config/forgejo-agent/config.env
sudo chmod 0644 /home/codex-dev/.config/forgejo-agent/config.env

sudo tee /home/codex-orch/.config/forgejo-agent/config.env >/dev/null <<'EOF'
FORGEJO_BASE_URL=http://127.0.0.1:3000
FORGEJO_DEFAULT_OWNER=main
FORGEJO_DEFAULT_REPO=forgejo-work
FORGEJO_AGENT_NAME=codex-orch
FORGEJO_LEASE_MINUTES=90
EOF
sudo chown codex-orch:codex-orch /home/codex-orch/.config/forgejo-agent/config.env
sudo chmod 0644 /home/codex-orch/.config/forgejo-agent/config.env
```

Launch an isolated interactive dev session:

```bash
sudo systemd-run \
  --unit=codex-dev-$(date +%s) \
  --collect \
  --wait \
  --pty \
  -p User=codex-dev \
  -p WorkingDirectory=/home/main/programming/projects/your-repo \
  -p LoadCredential=forgejo_token:/etc/forgejo-agent/creds/codex-dev.token \
  /usr/bin/bash -lc 'exec codex'
```

`forgejoctl` in that session automatically resolves token from
`$CREDENTIALS_DIRECTORY/forgejo_token`.
