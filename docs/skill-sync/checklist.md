# Skill/API Surface Sync Checklist

CLI_SURFACE_SHA256: 89f0101443949bd7342d3a00400e73857d6d7a207e40055899c24c962ca98266

- [x] Verified skill command reference against docs/skill-sync/cli-surface.txt.
- [x] Updated skill procedural guidance for any CLI/workflow changes.
- [x] Re-ran python3 /home/main/forgejo-agent/scripts/check.py after sync updates.

Use this checklist with:

```bash
python3 scripts/verify_skill_sync.py --update
python3 scripts/check.py
```
