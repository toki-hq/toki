---
description: Cut a Toki release — verify, (bump if needed), GitHub release + tag, watch the release CI, draft the French Discord message.
argument-hint: <version>  e.g. 0.5.1  or  v0.6.0
---
Cut the Toki release for version **$ARGUMENTS**. Treat the leading `v` as optional in the argument; the git tag and Docker tags are always `vX.Y.Z` and `X.Y.Z` respectively. If $ARGUMENTS is empty or not a valid semver, stop and ask for the version.

This command follows the project's release flow (single source of truth: `version` in the root `Cargo.toml` `[workspace.package]`; all crates inherit via `version.workspace = true`). Do the prep and checks autonomously, then **PAUSE for explicit go-ahead before the irreversible step** (`gh release create` — it creates the public tag and triggers the Docker push). Use TodoWrite to track the phases.

Current state to read before you start:
!`cd "$(git rev-parse --show-toplevel)" && echo "=== branch + clean ===" && git branch --show-current && git status --short && echo "=== Cargo.toml version ===" && grep -m1 '^version' Cargo.toml && echo "=== latest tags ===" && git tag --sort=-v:refname | head -5 && echo "=== last commit ===" && git log --oneline -1`

## 1. Preconditions (abort with a clear message if any fail)
- Be on `main`, working tree clean (ignore untracked `design_handoff*`, `*.zip`, `.claude/` extras — only *tracked* changes block).
- `main` must be **pushed and up to date** with `origin/main`.
- The version tag `vX.Y.Z` must **not already exist** (`git tag | grep`). If it does, stop — the release was already cut.

## 2. Version bump (only if needed)
- If `Cargo.toml`'s version already equals X.Y.Z, **skip this phase** (it's pre-set, as is common here).
- Otherwise: set `version = "X.Y.Z"` in `Cargo.toml` `[workspace.package]`, run `cargo update -p toki-client -p toki-server -p toki-proto --precise X.Y.Z` to sync `Cargo.lock`, verify both files agree, commit as `chore(release): vX.Y.Z`, push to `main`, and **wait for the push CI to go green** before continuing. (The admin-ui `package.json` version tracks separately — do NOT bump it.)
- **CI gotcha**: if this release includes any `.proto` change since the last tag, confirm the admin-ui `src/gen` TS stubs were regenerated + committed — CI's `admin UI (build)` job guards against drift and will fail otherwise. (No `.proto` change → nothing to do.)

## 3. Confirm main is green
Check the most recent `push` CI run on `main` succeeded (`gh run list --branch main --limit 1`). If it's still running, wait. If it failed, stop and report — don't release on a red main.

## 4. Draft the release notes
Gather everything since the last release tag — `git log <last-tag>..main --oneline` — and the PR numbers (`grep -oE '#[0-9]+'`). Write `/tmp/RELEASE_NOTES_<version>.md` in the established format (see prior GitHub releases, e.g. `gh release view v0.5.0`):
- `## vX.Y.Z` heading + a one-line release character.
- **A compatibility line.** Check `proto::version::compatible` (it gates on **MAJOR.MINOR**). If this bump crosses a MINOR/MAJOR boundary OR there are wire-format changes in `crates/proto/src/` since the last tag, state clearly **"NOT wire-compatible with the previous line — upgrade server + all clients together"** (the version gate enforces a hard reject). If it's a patch bump with no wire change, state it **stays wire-compatible** with the same MAJOR.MINOR.
- `### Added` / `### Fixed` / `### Security` / `### Internal` sections, each bullet referencing its PR number, written for operators/users (not commit-speak).
- A final `**Docker:** ellessen/toki-server:X.Y.Z · ellessen/toki-admin-ui:X.Y.Z` line.

## 5. ⏸ PAUSE — show the notes, get the go-ahead
Show the drafted notes. Ask the user to confirm before publishing — this is the point of no return (public tag + Docker push to Docker Hub). Do not proceed without an explicit yes. (If they want edits, revise the notes file and re-confirm.)

## 6. Create the GitHub release (this creates the tag)
`gh release create vX.Y.Z --target main --title "<title>" --notes-file /tmp/RELEASE_NOTES_<version>.md`. The release must exist **first** — CI's `publish-release` job uses `gh release upload` (not create) to attach the client zips. Then `git fetch --tags origin` so the local tag matches.

## 7. Watch the release CI
The tag push triggers the tag-only jobs (`if: startsWith(github.ref, 'refs/tags/v')`): `docker-server-release` + `docker-admin-ui-release` (multi-arch → Docker Hub `X.Y.Z` + `latest`), the three `client (...)` builds (macOS `.app`, Windows `.exe`, Linux), and `publish-release`. Find that run (`gh run list --limit 5`, event=push, branch=`vX.Y.Z`) and watch it to completion (`gh run watch <id> --exit-status`) — it takes several minutes (Windows MSVC build + multi-arch Docker). When done, verify: every release job succeeded, the three client zips are attached (`gh release view vX.Y.Z --json assets`), and the Docker `X.Y.Z` tags exist. Report any failure with the job link.

## 8. Draft the French Discord announcement
There is **no Discord webhook/automation in the repo** — the user posts it manually, so just DRAFT it (write to `/tmp/DISCORD_FR_<version>.md` and show it). Match the FR radio voice: *fréquence/canal*, *"Maintenez pour parler, relâchez pour écouter"*, *talkie-walkie*, push-to-talk, half-duplex. Include: 3–6 feature highlights from the release notes (emoji bullets), the **upgrade-together warning** if it's a wire-compat break, the two Docker tags, and the release URL `https://github.com/toki-hq/toki/releases/tag/vX.Y.Z`. (Historical note: the old `docs/toki-presentation-fr.html` voice reference no longer exists — reconstruct the tone from the vocabulary above.)

## 9. Wrap up
Summarise: tag + release URL, Docker tags pushed, zips attached, and present the FR Discord draft for the user to copy-paste. Note anything that needs a manual follow-up.
