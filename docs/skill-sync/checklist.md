# Skill/API Surface Sync Checklist

CLI_SURFACE_SHA256: aadf779ef307c6f37042d4040e1ec54af0f2b4ec462a7e3df1828d6014994629

- [x] Verified skill command reference against docs/skill-sync/cli-surface.txt.
- [x] Updated skill procedural guidance for any CLI/workflow changes.
- [x] Re-ran python3 /home/main/forgejo-agent/scripts/check.py after sync updates.

Use this checklist with:

```bash
python3 scripts/verify_skill_sync.py --update
python3 scripts/check.py
```
