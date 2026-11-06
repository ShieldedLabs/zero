# Releasing Zero

This is the playbook for cutting a Zero release. Run the five steps below on cut day, in order.

> [!IMPORTANT]
> Versions are plain integers. One `vN` tag covers the whole source tree. In every command below, replace `vN` with the new version (for example `v30`).

## 1. Prep

Do the prep on a `release-prep-vN` branch and land it through one PR. The PR holds the height edits from the checklist below and the changelog cut.

```sh
git switch main && git pull
git switch -c release-prep-vN
```

### Pre-release checklist

zcashd and zebra halt on mainnet at a hardcoded height. The height moves only when someone edits it. Set both before the changelog cut.

1. **zcashd.** Set `APPROX_RELEASE_HEIGHT` in `zcashd/src/deprecation.h` to the current mainnet height plus a ship-day buffer. Allow about 1,150 blocks per day. The halt comes 7 weeks of blocks (56,448) later. Put that height and its approximate date in the changelog entry, as v28 does. The same constant seeds the default `signrawtransaction` branch id, so keep it at or below the activation height of any pending upgrade.
2. **zebra.** The halt is `ESTIMATED_RELEASE_HEIGHT` plus `EOS_PANIC_AFTER` days of blocks, both in `zebra/zebrad/src/components/sync/end_of_support.rs`. Zero carries that height as a `[zero]` line pinned to the value from the newest upstream zebra release. Re-set it or pull that tag. Then check that the halt lands well after the next planned release.

### Cut the changelog

Notes collect under `## Unreleased` in `CHANGELOG.md` until this commit retitles them. The script renames `## Unreleased` to `## vN - YYYY-MM-DD`, adds a new empty `## Unreleased` above it, and commits. It stops while other tracked files are dirty. It prints the new section, so read it before pushing.

```sh
.github/scripts/cut-changelog.sh vN
git push -u origin HEAD
gh pr create --base main --title "release-prep-vN" --fill
```

## 2. GitHub

### Pin the release commit

The release is exactly the commit the prep PR landed on `main`. Merge the PR first, then push a `release-vN` branch at it. GitHub dispatches a workflow on a branch or tag name, so the branch carries the SHA.

```sh
git fetch origin
SHA=$(git rev-parse origin/main)   # must be the commit the prep PR landed
git log -1 --oneline "$SHA"
git push origin "$SHA:refs/heads/release-vN"
```

Delete `release-vN` after the release is out.

### Check the zeronym builds

A red reproduce run blocks nothing on its own, so check both builds for the release commit. For each workflow, the script reads the push paths from the workflow file at `$SHA` and finds the last commit at or before `$SHA` that touches them. It then looks for a green run that proves that commit.

- A green `push` run on that commit counts.
- A green `workflow_dispatch` run on `$SHA` also counts. Use it when the push run was cancelled by a later push.

```sh
.github/scripts/release.sh check-reproduce "$SHA"
```

Both lines must say `ok`. On `STOP`, dispatch that workflow on `release-vN` and run the check again once it passes.

```sh
.github/scripts/release.sh dispatch zeronym-hub-reproduce.yml release-vN   # or the shim one
```

### Cut the release

Dispatch `release.yml` on `release-vN` and watch it. With a blank `version` input it releases the highest `v<integer>` tag plus one. So the `## vN` heading must use that integer. It refuses a pre-release zebra and any halt under 4 weeks past the mainnet tip. Then it runs a full `z3-smoke` run, then creates the tag and the release.

```sh
.github/scripts/release.sh dispatch release.yml release-vN
```

The script waits for the run this dispatch created, watches it, and prints its `headSha`. It must equal `$SHA`. `release.yml` tags the commit it ran on. Stop if it differs.

### Publish the images

Run `images.yml` on the new tag. The `tag` input makes it push `:vN` beside `:latest`.

```sh
.github/scripts/release.sh dispatch images.yml vN -f tag=vN
```

### Verify the release

The release page should show the changelog section as its body, plus these assets.

- Seven source ZIPs, one per component (`zcashd-vN.zip` and so on)
- `zebrad-vN-linux-x86_64` and its `.sha256`
- The smoke-test image exports `zero-zebra.tzst`, `zero-zaino.tzst`, `zero-zallet.tzst` and four `ShieldedLabs.zero.*.dockerbuild` files

```sh
gh release view vN
```

Each image on GHCR should carry the `:vN` tag.

```sh
for img in zero-zebra zero-zcashd zero-zaino zero-zallet zero-lightwalletd; do
  if docker manifest inspect "ghcr.io/shieldedlabs/$img:vN" >/dev/null; then echo "$img ok"; else echo "$img MISSING"; fi
done
```

Every image must print `ok`.

### Add the zeronym pins

Do this when the release carries zeronym changes.

The release lists the reproducible-build pins from the two `EXPECTED_SHA256` files, and a live enclave's hash lives in `PROVENANCE` on its `deploy-<app-id>` tag.

`gh release edit --notes-file` replaces the whole body. So fetch the current body first, append the block, and write the result back.

```sh
git fetch -q origin tag vN
sha=$(git rev-parse 'vN^{commit}')
hub=$(git show "$sha:zeronym/hub/deploy/EXPECTED_SHA256")
shim=$(git show "$sha:zeronym/shim/deploy/EXPECTED_SHA256")
gh release view vN --json body --jq .body > notes.md
cat >> notes.md <<EOF

## zeronym reproducible-build pins

These are the reproducible-build pins at [vN](https://github.com/ShieldedLabs/zero/tree/vN) (\`$sha\`).

- Hub \`$hub\` ([EXPECTED_SHA256](https://github.com/ShieldedLabs/zero/blob/$sha/zeronym/hub/deploy/EXPECTED_SHA256))
- Shim \`$shim\` ([EXPECTED_SHA256](https://github.com/ShieldedLabs/zero/blob/$sha/zeronym/shim/deploy/EXPECTED_SHA256))

A live enclave runs the hash in \`PROVENANCE\` on its \`deploy-<app-id>\` tag. Check that file to see what a given endpoint runs.
EOF
```

Check that `notes.md` starts with the full body from `release.yml`. Then write it back.

```sh
gh release edit vN --notes-file notes.md
```

## 3. Forum and X

Post a new thread for each release on the [Zcash Community Forum](https://forum.zcashcommunity.com/).

Post two lines and the release link on X from the Shielded Labs account.

For a release with zeronym changes, add one sentence on rollout order and any new required flags. Point readers at the [hub operator guide](zeronym/hub/deploy/caution/OPERATORS.md) and the [shim operator guide](zeronym/shim/deploy/caution/OPERATORS.md).

## 4. Signal

Post a short version of the forum text to the operator Signal group.

## 5. Blog and dashboard

**Blog.** This step is optional. Write a post on [shieldedlabs.net](https://shieldedlabs.net) when a release changes what operators or users do.

**Dashboard.** For a release with zeronym changes, notify the [indexer dashboard](https://zecblock.com/network/attestations) maintainers and send them the tag, the commit and both `EXPECTED_SHA256` values at that commit.

