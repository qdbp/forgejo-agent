# Skill/API Surface Sync Checklist

CLI_SURFACE_SHA256: 19a57bba6a5478709067ed0b4010f9ac0e67c4c0d91d500db33d43272128d3ed

- [x] Verified skill command reference against docs/skill-sync/cli-surface.txt.
- [x] Updated skill procedural guidance for any CLI/workflow changes.
- [x] Re-ran python3 /home/main/forgejo-agent/scripts/check.py after sync updates.

Use this checklist with:

```bash
python3 scripts/verify_skill_sync.py --update
python3 scripts/check.py
```
