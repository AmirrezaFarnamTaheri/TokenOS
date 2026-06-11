# Staged GitHub Actions workflows

The CI/release pipeline lives here instead of `.github/workflows/` because
the automation token that pushes this branch lacks the `workflows`
permission — GitHub rejects any push that creates or updates files under
`.github/workflows/` from such a token.

## Activation (one command, run by a human with normal repo access)

```sh
git mv .github/workflows-staged/ci.yml .github/workflows/ci.yml
git commit -m "ci: activate build/release pipeline"
git push
```

(or simply copy `ci.yml` into `.github/workflows/` via the GitHub web UI —
"Add file" there grants the workflow scope implicitly.)

## What `ci.yml` does

| Job | Trigger | Purpose |
|---|---|---|
| `test` | every push / PR | fmt + clippy (informative), `cargo build --release --locked`, full test suite (headless, no GUI deps) |
| `native` | after `test` | 3-OS matrix (Ubuntu / macOS / Windows) building `--features native` — installs WebKitGTK on Linux, smoke-tests the binary, uploads per-platform artifacts |
| `release` | tags `v*` | packages all platform artifacts as tarballs and attaches them to the GitHub release |
