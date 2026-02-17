PR landing blocked: merge still not possible after rebase.

- PR: {{pr_url}}
- branch: `{{branch}}` (base `{{base_branch}}`)

Next action:
- Rebase manually and push: `git fetch origin {{base_branch}} && git rebase origin/{{base_branch}} && git push --force-with-lease origin HEAD:{{branch}}`
- Then retry by commenting: {{retry_mention}}

Error:
{{error}}
