# Signing and publishing the printer list

The app only trusts a printer list (`manifest/manifest.json`) whose minisign signature verifies
against the public key compiled into it (`MANIFEST_PUBKEY` in `crates/core/src/keys.rs`).
`.github/workflows/manifest.yml` lints the list on every push and pull request, and on a push
to `main` signs it and uploads it to `installer/` in the release bucket, after a reviewer
approves the `manifest-publish` environment. Until the steps below are done the publish job
fails closed: it signs and uploads nothing.

Signatures must be prehashed (`minisign -S -H`); the app refuses legacy ones, and the workflow
checks every signature with the app's own verifier before it uploads.

## 1. Generate the key (offline machine, once)

```sh
minisign -G -p helix-installer.pub -s helix-installer.key
```

Choose a strong password when asked. Keep `helix-installer.key` and the password offline
(password manager plus an offline backup); only the CI copy below ever goes online. The
`.pub` file is public.

## 2. Compile the public key into the app

Paste the second line of `helix-installer.pub` (the base64 key, starting `RW`) into
`crates/core/src/keys.rs`:

```rust
pub const MANIFEST_PUBKEY: &str = "RW...";
```

Commit it. Every installer build from then on trusts that key. Changing the key later means
shipping a new app build, so treat it as permanent.

## 3. Create the environment with a required reviewer

```sh
gh api -X PUT repos/prestonbrown/helix-installer/environments/manifest-publish \
  -F "reviewers[][type]=User" -F "reviewers[][id]=$(gh api user --jq .id)" \
  -F "deployment_branch_policy[protected_branches]=false" \
  -F "deployment_branch_policy[custom_branch_policies]=true"
gh api -X POST repos/prestonbrown/helix-installer/environments/manifest-publish/deployment-branch-policies \
  -f name=main
gh api -X POST repos/prestonbrown/helix-installer/environments/manifest-publish/deployment-branch-policies \
  -f name='v*' -f type=tag
```

The `main` policy admits the printer-list publish. The `v*` tag policy admits the release
workflow's `publish-updater` job, which runs from the release tag (`refs/tags/v...`), not a
branch; without it that job is never allowed to deploy. The printer-list job still runs only on
a push to `main` (its own `if:`), so the tag policy opens the environment to nothing else.

## 4. Add the secrets to that environment

Environment secrets are only readable by a publish job a reviewer has approved.

```sh
R=prestonbrown/helix-installer
gh secret set MANIFEST_SIGNING_KEY --env manifest-publish -R $R < helix-installer.key
gh secret set MANIFEST_SIGNING_KEY_PASSWORD --env manifest-publish -R $R   # prompts
gh secret set R2_ACCESS_KEY_ID --env manifest-publish -R $R
gh secret set R2_SECRET_ACCESS_KEY --env manifest-publish -R $R
gh secret set R2_ACCOUNT_ID --env manifest-publish -R $R
```

The R2 values are the same ones the HelixScreen release workflow uses; the token needs write
access to the release bucket. Optional variables (environment or repository): `R2_BUCKET_NAME`
(default `helixscreen-releases`) and `R2_PUBLIC_URL` (`https://releases.helixscreen.org`; when
set, the job downloads what it published and verifies it again).

## 5. Turn publishing on

The publish job is skipped until this repository variable is set, so CI stays green before
signing is configured:

```sh
gh variable set MANIFEST_PUBLISH_ENABLED --body true -R prestonbrown/helix-installer
```

## 6. Publish

Merge to `main`, approve the `manifest-publish` deployment when GitHub asks. The job writes
`installer/manifest.json`, `installer/manifest.json.minisig` and `installer/img/`. The app
fetches `https://releases.helixscreen.org/installer/manifest.json`; set
`HELIX_INSTALLER_MANIFEST` to point a development build elsewhere.

## Checking a signature by hand

```sh
cargo run -p helix_installer_core --bin manifest-lint -- --verify manifest.json \
  --sig manifest.json.minisig --pubkey helix-installer.pub
```

# Signing the app and the updater

`.github/workflows/release.yml` runs on a `v*` tag, or by hand (`gh workflow run release.yml
--ref engine`), and leaves a **draft** release with the Windows `.msi` and `.exe`, the macOS
`.dmg` (universal) and the Linux AppImage and `.deb`. Every signing step switches on only
when its secrets exist; with none set the build still succeeds and each job's summary lists
what came out unsigned. Nothing below is required to build.

The signing secrets reach only the step that checks which of them are set and the Tauri build
step itself, through `scripts/tauri-signed.cjs`, which passes on only the ones switched on. No
dependency install, cache or toolchain action runs with them in its environment. The Rust and
npm builds that run inside the Tauri step do see them; `Cargo.lock` and `package-lock.json` pin
what those are. Every third-party action is pinned to a commit.

Set repository secrets with `gh secret set NAME -R prestonbrown/helix-installer`.

## In-app updater

The app checks `https://releases.helixscreen.org/installer/app/latest.json` once at startup,
but only when `UPDATER_PUBKEY` in `crates/core/src/keys.rs` is set. Empty (today), it never
checks and shows nothing.

```sh
cd ui && npx tauri signer generate -w ~/helix-installer-updater.key
```

Choose a password. The command prints the public key and writes the private one to the file.

```sh
R=prestonbrown/helix-installer
gh secret set TAURI_SIGNING_PRIVATE_KEY -R $R < ~/helix-installer-updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD -R $R   # prompts
```

Paste the public key into `UPDATER_PUBKEY` and commit; builds from then on check for updates.
Changing it later strands every installed copy, so treat it as permanent and back the private
key up offline.

With `TAURI_SIGNING_PRIVATE_KEY` set the release builds updater bundles and `.sig` files, and
the `publish-updater` job writes `latest.json` plus the bundles to `installer/app/<version>/` in
the release bucket, after you approve the `manifest-publish` deployment (same environment and R2
secrets as the printer list above; the environment needs the `v*` tag policy from step 3). The
version comes from `crates/app/tauri.conf.json`, and the job refuses a tag that does not name it
(`v0.2.0` for `"version": "0.2.0"`). Bump the version before tagging.

Two things to know before approving that deployment:

- **Approving it ships the update.** Every installed copy sees `latest.json` within minutes,
  even though the GitHub release is still a draft. Check the draft first and approve only when
  you mean to release.
- **The updater does not check the version signature** (`requireSignedVersion`, unset under
  `plugins.updater` in `crates/app/tauri.conf.json`, is off by default). It verifies the downloaded bundle's signature, but anyone able
  to serve a forged `latest.json` could pair a higher version number with an older, genuinely
  signed bundle. Set it to `true` once every release users can update from was signed by a
  Tauri CLI that records the version in the signature.

The updater key is a repository secret, so any run of the release workflow (a tag, a manual
dispatch on any branch) produces validly signed bundles; only `latest.json`, behind the
environment, decides what users get.

## Apple (on hold)

Needs the Apple Developer Program membership and a "Developer ID Application" certificate
exported from Keychain as a `.p12`.

```sh
base64 -i DeveloperID.p12 | gh secret set APPLE_CERTIFICATE -R $R
gh secret set APPLE_CERTIFICATE_PASSWORD -R $R   # the .p12 export password
gh secret set APPLE_SIGNING_IDENTITY -R $R       # "Developer ID Application: Name (TEAMID)"
gh secret set APPLE_ID -R $R                     # Apple ID email, for notarization
gh secret set APPLE_PASSWORD -R $R               # an app-specific password from appleid.apple.com
gh secret set APPLE_TEAM_ID -R $R
```

The first three sign the `.dmg`; the last three also notarize it. Without all of them the
`.dmg` is unsigned and Gatekeeper blocks it.

## Windows: Azure Artifact (Trusted) Signing

Create an Artifact Signing account and a certificate profile in the Azure portal, complete
identity validation, then register an app (service principal) with the "Artifact Signing
Certificate Profile Signer" role on the account.

```sh
gh secret set AZURE_TENANT_ID -R $R
gh secret set AZURE_CLIENT_ID -R $R
gh secret set AZURE_CLIENT_SECRET -R $R
gh secret set AZURE_SIGNING_ENDPOINT -R $R   # e.g. https://wus2.codesigning.azure.net
gh secret set AZURE_SIGNING_ACCOUNT -R $R    # the account name
gh secret set AZURE_SIGNING_PROFILE -R $R    # the certificate profile name
```

With all six set, the Windows job installs `artifact-signing-cli` and Tauri signs the `.msi`
and `.exe` through it (`bundle.windows.signCommand`). Without them they are unsigned and
SmartScreen warns.
