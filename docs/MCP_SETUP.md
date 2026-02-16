# MCP Setup (Optional, Recommended)

This gives Codex direct tools for Forgejo issues/labels/milestones/etc.

## 1) Install MCP server binary without Go toolchain

```bash
# Arch (AUR binary package, no Go toolchain needed)
yay -S forgejo-mcp-bin
```

If you install manually, place the binary at `~/.local/bin/forgejo-mcp` and make it executable.

Wrapper resolution order:

1. `FORGEJO_MCP_BIN` from config/env
2. `forgejo-mcp` on `PATH`
3. `~/.local/bin/forgejo-mcp`
4. legacy fallback `~/go/bin/forgejo-mcp` (deprecated compatibility path)

## 2) Use token-aware wrapper

Wrapper script already exists:

- `/home/main/forgejo-agent/bin/forgejo-mcp-stdio`

It reads:

- `~/.config/forgejo-agent/config.env`
- token from `FORGEJO_TOKEN_FILE`

Then starts MCP in stdio mode with environment variables.

If needed, pin a custom binary path in `~/.config/forgejo-agent/config.env`:

```bash
FORGEJO_MCP_BIN=/absolute/path/to/forgejo-mcp
```

## 3) Add to Codex config

Add this block to `~/.codex/config.toml`:

```toml
[mcp_servers.forgejo]
command = "/home/main/forgejo-agent/bin/forgejo-mcp-stdio"
startup_timeout_sec = 15
tool_timeout_sec = 120
```

## 4) Validate

```bash
codex mcp list
```

Then in a Codex session, ask it to list/open Forgejo issues using MCP tools.

## 5) Optional migration away from `~/go` (recommended)

If you previously used `go install`, move the binary:

```bash
mkdir -p ~/.local/bin
install -m 0755 ~/go/bin/forgejo-mcp ~/.local/bin/forgejo-mcp
```

After confirming MCP works, you can remove Go workspace remnants:

```bash
rm -rf ~/go
```

## Security notes

- Keep Forgejo bound to `127.0.0.1`.
- Keep token file mode `0600` and config dir mode `0700`.
- Prefer one token per automation role (worker token, admin token) and rotate periodically.
