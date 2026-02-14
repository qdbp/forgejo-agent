# Skill/API Surface Sync Checklist

CLI_SURFACE_SHA256: acbaaef2a94786ff55e40cd30710bc987bf427393cb0a9ed10503c5a7fd5142e

- [x] Verified skill command reference against docs/skill-sync/cli-surface.txt.
- [x] Updated skill procedural guidance for any CLI/workflow changes.
- [x] Re-ran python3 /home/main/forgejo-agent/scripts/check.py after sync updates.

Use this checklist with:

```bash
python3 scripts/verify_skill_sync.py --update
python3 scripts/check.py
```
