# Maintaining Jake's Hex fork

This repository is [jakebodea/hex](https://github.com/jakebodea/hex), forked from
[anomalyco/hex](https://github.com/anomalyco/hex). The original MIT license and
attribution remain intact. `main` contains the personal app; `origin` points to
the fork and `upstream` points to the original repository. Local upstream pushes
are disabled, and the default push remote and GitHub CLI repository are the fork.

## Personal changes

- Dark capsule with white waveform bars, immediate volume attacks, short release,
  and a fast outward ripple. HUD Lab clears its preview when leaving the pane.
- Codex and Claude Code CLI rewriting alongside OpenCode, including provider/model
  settings and install/sign-in status. See [AI rewriting](features/README.md#ai-rewriting-providers).
- Shared destination-aware rewrite prompts, optional window-title sharing, and a
  focus guard that holds rewritten text for Paste Last when another app takes focus.
- Rewrite rules that repair misheard speech recognition, plus a per-mode
  Vocabulary list (Global applies everywhere) the rewriter prefers for names and
  jargon, so smaller local speech models stay usable.
- Apple Speech re-enabled in the macOS model picker as the "Lowest memory"
  choice. The installer copies permission descriptions from `app/Info.plist`
  into the retained bundle so Speech Recognition access can be requested.
- An Apple Development signed local installer that retains the installed app's
  resources and turns off Sparkle updates so the upstream feed cannot replace it.

## Validate and install

Use the same macOS deployment target for builds and checks to share the cache:

```sh
export MACOSX_DEPLOYMENT_TARGET=15.0
./scripts/test-personal-signing.sh
cargo fmt --check
cargo test --locked --release --bin voice-control
cargo clippy --locked --release --bin voice-control --tests -- -D warnings
git diff --check
./scripts/build-personal-app.sh --install
```

The installer needs an existing `/Applications/Hex.app` as its resource template
and its existing Apple Development signing identity in Keychain. Updates reuse
that exact signer and verify the new app against the previous designated
requirement before installation. A missing key fails instead of selecting a
different certificate, because macOS privacy grants depend on the signer.
Set `HEX_SIGN_IDENTITY` explicitly for the first transition from an upstream
release, or an intentional identity repair; `HEX_BASE_APP` overrides the template. It moves the previous app to the
Trash before replacement and relaunches Hex. This is a personal build, not a
notarized public release; no updater feed or release credentials are configured
for this fork. The README's public downloads still refer to upstream binaries.

## Bring in upstream fixes

Start with a clean working tree on `main`. Fetch and review the incoming changes,
then merge deliberately; do not reset away the personal commits:

```sh
git switch main
git fetch upstream
git log --oneline main..upstream/main
git merge upstream/main
```

Resolve any conflicts while preserving the personal features, run validation,
then push and reinstall:

```sh
git push origin main
./scripts/build-personal-app.sh --install
```

Keep shared infrastructure and provider changes separate from personal app work.
The inherited site-deploy, npm-release, and desktop-release jobs are restricted
to `anomalyco/hex`; normal check workflows remain available to this fork.
