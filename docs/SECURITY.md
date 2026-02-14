# Security Hardening Notes

## What is hardened by default here

- Forgejo binds to `127.0.0.1` only.
- Registration and public unauthenticated browsing are disabled.
- Built-in SSH, actions, package registry, and OAuth/OpenID are disabled.
- Repo defaults are private.
- Password hashing is `argon2`.
- Token storage is file-based with strict perms (`0700` dir, `0600` file).

## Arch service hardening

The packaged systemd unit already includes strong sandboxing (`ProtectSystem=strict`, `NoNewPrivileges`, private `/tmp`, restricted syscalls, etc.).

## Additional recommended controls

- Keep system firewall default deny for inbound except explicitly needed ports.
- Use dedicated token per automation role and rotate quarterly.
- Keep admin token offline; use narrower-scope worker token for bots.
- Back up `/etc/forgejo/app.ini` and `/var/lib/forgejo/data/forgejo.db`.
- If you ever expose Forgejo beyond localhost, terminate TLS and enforce MFA.
