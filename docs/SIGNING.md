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
```

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

## 5. Publish

Merge to `main`, approve the `manifest-publish` deployment when GitHub asks. The job writes
`installer/manifest.json`, `installer/manifest.json.minisig` and `installer/img/`. The app
fetches `https://releases.helixscreen.org/installer/manifest.json`; set
`HELIX_INSTALLER_MANIFEST` to point a development build elsewhere.

## Checking a signature by hand

```sh
cargo run -p helix_installer_core --bin manifest-lint -- --verify manifest.json \
  --sig manifest.json.minisig --pubkey helix-installer.pub
```
