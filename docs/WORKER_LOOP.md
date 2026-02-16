# Work-On-Issues Loop

## One-shot dry run

```bash
/home/main/.local/bin/forgejoctl worker run \
  --repo main/backlog \
  --once
```

This picks one `state/ready` issue (without `claimed/*`), prints the Codex prompt, then releases claim.

## Active execution loop

```bash
/home/main/.local/bin/forgejoctl worker run \
  --repo main/backlog \
  --workdir /home/main/programming/projects/your-repo \
  --execute \
  --interval-sec 45 \
  --agent codex-dev
```

Optional: auto-close issues after successful run.

```bash
... --close-on-success
```

## User-level systemd service (optional)

```bash
mkdir -p ~/.config/systemd/user
cp /home/main/forgejo-agent/templates/codex-issue-worker.service ~/.config/systemd/user/
# edit ExecStart repo/workdir/agent before enabling
systemctl --user daemon-reload
systemctl --user enable --now codex-issue-worker.service
systemctl --user status codex-issue-worker.service --no-pager
```

If user services are disabled outside login sessions, enable linger:

```bash
sudo loginctl enable-linger "$USER"
```
