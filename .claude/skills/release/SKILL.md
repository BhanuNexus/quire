---
name: release
description: Release a Quire version (bump, dry run, tag, publish) by following RELEASING.md. Runs only when asked.
disable-model-invocation: true
argument-hint: "[version, for example 0.14.0]"
---

Release Quire $ARGUMENTS. The steps are in `RELEASING.md`, section "Release checklist for an agent" (nine steps, in order); the rest of that file explains them. This skill adds three rules and does not repeat the steps, so there is one source.

1. **A tag is used once.** Never delete, move or reuse a tag, even if the release run failed. If a release fails after its tag exists, fix the problem and release the next version.
2. **Run Tag release with `check_only` first** (step 6): `gh api repos/BhanuNexus/quire/actions/workflows/tag-release.yml/dispatches -f ref=main -f 'inputs[version]=X.Y.Z' -f 'inputs[check_only]=true'`. It must be green before the real run. It creates nothing.
3. **Stop and ask before tagging** (step 7, the run without `check_only`). Show the person: the version, the commit, the green dry run and the green check-only run, and wait for a yes in this conversation. A tag cannot be taken back.

Also:
- Do the dry run (step 5) first, and read every job to the end.
- Do not release from a commit that `CI` and `CodeQL` have not passed on `main`; the guard checks it, and the release's `check` job checks it again.
- Do not push, tag or dispatch anything without being asked to release.
- Edit the file set in `packaging/release-files.txt` in the same commit when a package is added or dropped (step 3).
