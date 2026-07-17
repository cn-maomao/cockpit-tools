# Fork release setup

The fork publishes and serves updates from `cn-maomao/cockpit-tools`.

## One-time updater signing setup

Tauri updater artifacts must be signed with the private key matching the public key in
`src-tauri/tauri.conf.json`. The private key must never be committed.

The generated fork key files are stored locally under the ignored directory:

```text
target/fork-release-signing/updater.key
target/fork-release-signing/updater.key.password
```

Back up both files securely, then add their contents as repository Actions secrets:

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY < target/fork-release-signing/updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD < target/fork-release-signing/updater.key.password
```

Losing the key or password prevents installed copies from accepting future updates.

## Automatic release behavior

`.github/workflows/sync-upstream.yml` synchronizes `jlcodes99/cockpit-tools:main` into the
fork. After a successful synchronization:

- if `v<package.json version>` has no fork Release, the workflow creates the tag and dispatches
  `.github/workflows/release.yml`;
- if that Release already exists, it dispatches the unsigned Build Matrix instead of replacing
  an existing release;
- scheduled runs with no upstream changes do not consume build minutes.

The Release workflow supports a fork's first release by publishing a temporary empty-platform
`latest.json`, then replacing it with the complete signed updater manifest after every platform
finishes.
