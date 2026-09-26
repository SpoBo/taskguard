---
name: release
description: Cut a taskguard release (version bump, tag, prebuilt binaries on GitHub) and optionally move DALP's pinned taskguard to it. Use when asked to release, publish, tag, or ship a new taskguard version, or to bump taskguard in DALP.
---

# Release taskguard

A release is a version bump plus a `v*` tag. The tag starts
`.github/workflows/release.yml`. That workflow builds four binaries and
publishes them on a GitHub release:

- `taskguard-aarch64-apple-darwin.tar.gz`
- `taskguard-x86_64-apple-darwin.tar.gz`
- `taskguard-x86_64-unknown-linux-musl.tar.gz`
- `taskguard-aarch64-unknown-linux-musl.tar.gz`

Each file has a `.sha256` file next to it. The same version also goes to
crates.io (see the last step). Publish there only when the user asks.

## Before you start

1. Ask the user before you push a tag. A tag makes a public release, and you
   cannot take it back cleanly.
2. Pick the version. Use semver: patch for fixes, minor for new flags or
   views. Before 1.0, a breaking CLI change is a minor bump.
3. Check that the state formats are still compatible. Old and new versions
   share one state directory (see "Mixed versions" in `README.md`). The tests
   `entry_format_is_stable`, `schema_stays_compatible` and `long_keys_are_hashed`
   guard this. Never change their pinned values to make them pass. If a change
   cannot keep these formats, it needs a new state directory. Stop and ask
   the user.

## Steps

`cargo` may not be on PATH (rustup from Homebrew). If so, run
`export PATH="$(dirname "$(rustup which cargo)"):$PATH"`.

```sh
git switch master && git pull --ff-only
git status --short                      # must be empty

cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

# Bump the version in Cargo.toml. The build updates Cargo.lock too.
sed -i '' 's/^version = "OLD"/version = "NEW"/' Cargo.toml   # GNU sed: sed -i
cargo build --release
grep -A1 'name = "taskguard"' Cargo.lock   # shows NEW

git add Cargo.toml Cargo.lock
git commit -m "chore: release NEW"
git tag vNEW
git push origin master vNEW              # only after the user said yes
```

Then watch the workflow, and check that all eight files are on the release:

```sh
id=$(gh run list -R SpoBo/taskguard --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId')
gh run watch "$id" -R SpoBo/taskguard --exit-status
gh release view vNEW -R SpoBo/taskguard --json assets --jq '.assets[].name'
```

To publish on crates.io, ask the user for a token. Pass it in the
environment only; never write it to a file:

```sh
cargo publish --dry-run                  # checks the package first
CARGO_REGISTRY_TOKEN=... cargo publish
```

`include` in `Cargo.toml` keeps the crate small. Add a file there when the
build or the tests need it.

A local `rust-objcopy ... libLLVM.dylib` warning on macOS comes from the
local toolchain. It does not affect the release: CI builds the binaries.

### Dry run

To test changes to `release.yml` without a release, start the workflow by
hand: `gh workflow run release.yml -R SpoBo/taskguard --ref master`. It builds
the same binaries and keeps them as workflow artifacts. It makes no release.

### When the workflow fails

Fix the cause on `master`. Then move the tag to the fixed commit, and push it
again. Do this only if the release does not exist yet:

```sh
git tag -f vNEW && git push -f origin vNEW
```

If a release was already published, do not move the tag. Make a new patch
version.

## Move DALP to the new version

DALP installs taskguard through devenv, from
`.agents/skills/throttle/taskguard.nix`. That file pins `version` and one
hash for each platform. Change it on a DALP branch, not on `main`.

1. Get the SRI hashes of the four archives:

   ```sh
   tmp=$(mktemp -d) && cd "$tmp"
   gh release download vNEW -R SpoBo/taskguard -p '*.tar.gz'
   for f in *.tar.gz; do echo "$f sha256-$(openssl dgst -sha256 -binary "$f" | base64)"; done
   ```

2. In `taskguard.nix`, set `version = "NEW";`. Then give each platform the
   hash of its archive:
   - `aarch64-darwin` gets `aarch64-apple-darwin`
   - `x86_64-darwin` gets `x86_64-apple-darwin`
   - `x86_64-linux` gets `x86_64-unknown-linux-musl`
   - `aarch64-linux` gets `aarch64-unknown-linux-musl`
3. Check that devenv installs it:

   ```sh
   devenv shell -- bash -c 'command -v taskguard; taskguard version'
   ```

   This must print a `/nix/store/...-taskguard-NEW/` path and `taskguard NEW`.
   A hash error means one hash is wrong. Nix prints the hash it expected.
4. Run the throttle tests:
   `devenv shell -- bunx vitest run --dir .agents/skills/throttle/scripts`.
5. Commit with a message like `chore(agents): taskguard NEW`. In the body, say
   in one line what the release fixes. If the PR text names the old version,
   update it.

Do not run whole-repo turbo tasks in DALP to test a release. A full run once
froze the machine. Use small `--filter` sets, and ask the user first.
