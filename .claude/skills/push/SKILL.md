---
name: push
description: Commit and push changes to GitHub
disable-model-invocation: true
argument-hint: [commit message]
---

1. Stage all relevant changed files (avoid secrets, .env, credentials)
2. Commit with message: $ARGUMENTS
3. Do NOT include any Co-Authored-By trailer or Claude attribution in the commit message
4. Push to the current branch
5. Retry up to 3 times on network failure with 5s backoff
6. Report the final status
