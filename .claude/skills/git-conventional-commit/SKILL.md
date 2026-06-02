---
name: git-conventional-commit
description: Generate and apply Conventional Commits-formatted git commit messages. Use this skill whenever the user wants to commit staged changes, write a commit message, push to GitHub, or says anything like "commit this", "write a commit message", "git commit", "push my changes", "stage and commit", or "what should my commit message be". Always trigger when there are staged git changes the user wants to commit.
---

# Git Conventional Commit

This skill generates well-formed [Conventional Commits](https://www.conventionalcommits.org/) messages from staged changes, optionally commits them, and offers to push.

## The Format

```
<type>(<scope>): <short description>

[optional body]

[optional footers]
```

A **breaking change** adds `!` after the scope and a `BREAKING CHANGE: <explanation>` footer:

```
feat(api)!: remove deprecated /v1/users endpoint

BREAKING CHANGE: /v1/users no longer exists. Migrate to /v2/users.
```

## Step 1 — Read the diff

Run `git diff --staged` (or `git diff --cached`) to see what's staged. Also run `git status` to confirm there are staged files; if there are none, tell the user and stop.

If the changes span many unrelated concerns, flag that to the user — a single commit should represent a single logical change. Don't try to produce multiple commit messages; just note it and let them decide.

## Step 2 — Identify the type

Pick the type that best describes the primary intent:

| Type | When to use |
|------|-------------|
| `feat` | A new feature or capability visible to users or callers |
| `fix` | Corrects a bug |
| `refactor` | Restructuring without changing behavior |
| `perf` | A change that improves performance |
| `test` | Adding or updating tests only |
| `docs` | Documentation only |
| `chore` | Build, tooling, dependency updates, CI config |
| `style` | Formatting, whitespace, naming — no logic change |
| `revert` | Reverts a previous commit |
| `ci` | CI/CD pipeline changes |

If nothing fits cleanly, use the closest match and note your reasoning.

## Step 3 — Pick a scope

The scope narrows what was changed. Do this in order:

1. **Check for a project config** — look for `commitlint.config.js`, `commitlint.config.ts`, `.commitlintrc`, `.cz.toml`, `.czrc`, or `pyproject.toml` (look for a `[tool.commitizen]` section). If one exists, read it and use only the scopes it defines.

2. **Infer from the diff** — if no config exists, derive the scope from what changed: the package name, the top-level directory, or the subsystem (e.g., `auth`, `api`, `ui`, `db`). Keep it lowercase, no spaces.

3. **Omit scope** if the change truly touches everything (e.g., a global rename) or if scope would be redundant with the type.

## Step 4 — Detect breaking changes

A change is breaking if it:
- Removes or renames a public function, class, method, route, or exported symbol
- Changes a function signature in a way callers must update
- Alters a database schema in a non-additive way
- Changes a config key that deployed systems depend on
- Drops support for a previously supported input

When you detect a breaking change: add `!` after the scope (e.g., `feat(api)!:`) and add a `BREAKING CHANGE: <clear explanation>` footer. Explain what broke and how callers should migrate.

## Step 5 — Look for issue references

Check whether:
- The user mentioned a ticket number in the conversation (e.g., "this fixes #42" or "related to PROJ-100")
- The branch name contains an issue number (run `git branch --show-current` and parse it)
- There's a `fixes`, `closes`, or `refs` pattern already in a WIP commit message

If you find a reference, add it as a footer:
- GitHub-style: `Fixes #42` (auto-closes the issue on merge)
- Jira-style: `Refs: PROJ-100`

## Step 6 — Write the message

**Subject line rules:**
- Lowercase after the colon, no period at end
- ≤ 72 characters total
- Imperative mood ("add", "fix", "remove" — not "added", "fixes", "removed")
- Be specific: "fix(auth): prevent token refresh on expired session" > "fix: bug fix"

**Body** (optional — include when the *why* isn't obvious from the diff):
- Blank line after subject
- Wrap at 72 characters
- Explain motivation and context, not mechanics

**Footers** — each on its own line after a blank line:
- `BREAKING CHANGE: ...`
- `Fixes #N` / `Closes #N`
- `Refs: TICKET-ID`
- `Co-authored-by: Name <email>`

## Step 7 — Confirm before committing

Show the user the proposed message and ask for confirmation:

```
Proposed commit message:

  fix(auth): prevent token refresh on expired session

  The refresh logic was called even when the session had already expired,
  causing a 401 loop on the next request.

Commit with this message? (yes / edit / cancel)
```

If they say yes, run:
```bash
git commit -m "<subject>" -m "<body>" -m "<footers>"
```
Or write to a temp file and use `git commit -F <file>` for multi-line messages with footers.

After committing, ask: "Push to remote? (yes / no / specify branch)"

## Edge cases

- **Nothing staged**: Tell the user clearly. Offer to show `git status`.
- **Merge commit / revert**: The message is usually auto-generated. Confirm before proceeding.
- **Monorepo with multiple packages changed**: Flag it — one commit per package boundary is usually better.
- **Already has a commit message in progress** (e.g., after `git commit --amend`): Respect what's there, offer to improve it rather than replace it wholesale.

## Examples

**Example 1 — simple fix:**
```
fix(db): handle null pointer in user lookup

getUserById() was not guarding against a missing record, causing a panic
when the user table was empty during tests.
```

**Example 2 — new feature with issue ref:**
```
feat(notifications): add email digest for weekly summaries

Refs: #217
```

**Example 3 — breaking change:**
```
feat(api)!: replace /users/:id with /v2/users/:uuid

BREAKING CHANGE: The /users/:id endpoint has been removed. All clients
must migrate to /v2/users/:uuid. UUIDs are available via the /v2/users
list endpoint.

Fixes #304
```

**Example 4 — chore:**
```
chore(deps): bump serde from 1.0.195 to 1.0.200
```
