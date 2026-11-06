# Releasing Zero

This is the playbook for cutting a Zero release. The release owner runs the five steps below on cut day, in order. A zeronym rollout follows them on its own schedule.

> [!IMPORTANT]
> Versions are plain integers. One `vN` tag covers the whole source tree. In every command below, replace `vN` with the new version (for example `v30`).

The release owner is the maintainer who lands the changelog retitle and dispatches `release.yml`.

## 1. Prep

Work through the pre-release checklist below and commit the height edits. Then land one commit that cuts the changelog. You can push it to `main` directly or open a `release-prep-vN` PR.

Notes collect under `## Unreleased` in `CHANGELOG.md` until this commit retitles them. The script renames `## Unreleased` to `## vN - YYYY-MM-DD`, adds a new empty `## Unreleased` above it, and commits. It stops while other tracked files are dirty. It prints the new section, so read it before you push.

```sh
git switch main && git pull
.github/scripts/cut-changelog.sh vN
git push
```

### Pre-release checklist

zcashd and zebra halt on mainnet at a hardcoded height. The height moves only when someone edits it. Set both before you retitle the changelog.

1. **zcashd.** Set `APPROX_RELEASE_HEIGHT` in `zcashd/src/deprecation.h` to the current mainnet height plus a ship-day buffer. Allow about 1,150 blocks per day. The halt comes 7 weeks of blocks (56,448) later. Put that height and its approximate date in the changelog entry, as v28 does. The same constant seeds the default `signrawtransaction` branch id, so keep it at or below the activation height of any pending upgrade.
2. **zebra.** The halt is `ESTIMATED_RELEASE_HEIGHT` plus `EOS_PANIC_AFTER` days of blocks, both in `zebra/zebrad/src/components/sync/end_of_support.rs`. Zero carries that height as a `[zero]` line pinned to the value from the newest upstream zebra release. Re-set it or pull that tag. Then check that the halt lands well after the next planned release.
3. **Preflight.** Run the same gates `release.yml` runs. It checks that zebra is a final version and that both halts sit at least 4 weeks past the mainnet tip.
   ```sh
   .github/scripts/release-preflight.sh
   ```

## 2. GitHub

### Pin the release commit

The release is exactly the prep commit. `main` takes direct pushes, so record that commit and push a branch that points at it. Every dispatch below runs on that branch. GitHub dispatches a workflow on a branch or tag name, so the branch carries the SHA.

```sh
git fetch origin
SHA=$(git rev-parse origin/main)   # must be your prep commit
git log -1 --oneline "$SHA"
git push origin "$SHA:refs/heads/release-vN"
```

Delete `release-vN` after the release is out.

### Cut the release

Dispatch `release.yml` on `release-vN` and watch it. With a blank `version` input it releases the highest `v<integer>` tag plus one. So the `## vN` heading must use that integer. It runs the preflight and a full `z3-smoke` run, then creates the tag and the release.

```sh
gh workflow run release.yml --ref release-vN
gh run watch
gh run list --workflow release.yml --limit 1 --json headSha,conclusion
```

The release run's `headSha` must equal `$SHA` too. `release.yml` tags the commit it ran on. Stop if it differs.

### Publish the images

Run `images.yml` on the new tag. The `tag` input makes it push `:vN` beside `:latest`.

```sh
gh workflow run images.yml --ref vN -f tag=vN
gh run watch
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
  docker manifest inspect "ghcr.io/shieldedlabs/$img:vN" >/dev/null && echo "$img ok"
done
```

### Add the zeronym pins

Do this when the release carries zeronym changes.

The release lists the reproducible-build pins from the two `EXPECTED_SHA256` files, and a live enclave's hash lives in `PROVENANCE` on its `deploy-<app-id>` tag.

`gh release edit --notes-file` replaces the whole body. So fetch the current body first, append the block, and write the result back.

```sh
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

Post a new thread for each release on the [Zcash Community Forum](https://forum.zcashcommunity.com/). Use this shape.

1. One paragraph that sums up the release.
2. Any action an operator must take.
3. The link to the release page.

We post on X from the Shielded Labs account. Write two lines and add the release link.

For a release with zeronym changes, add one sentence on rollout order and any new required flags. For v30 the clearnet shims with the strict lookup `404` deploy before the hub that closes `POST /transaction`, and the hub requires `--network` or `ZIH_NETWORK`. Point readers at the [hub operator guide](zeronym/hub/deploy/caution/OPERATORS.md) and the [shim operator guide](zeronym/shim/deploy/caution/OPERATORS.md).

## 4. Signal

Post a short version of the forum text to the operator Signal groups. The release owner keeps the list of group names outside the repo. The security disclosure waiting room is for reports only, so use the operator groups.

## 5. Blog and dashboard

**Blog.** This step is optional. A maintainer writes a post on [shieldedlabs.net](https://shieldedlabs.net) when a release changes what operators or users do.

**Dashboard.** For a release with zeronym changes, ask the maintainers for the contact who runs the indexer dashboard. Send them the tag, the commit and both `EXPECTED_SHA256` values at that commit. Steps 3 and 4 go out on cut day even if that contact is still unknown.

## Rolling out zeronym (zero-indexer) enclaves

This process is new and starts with v30. It runs after the five steps above and holds none of them. The maintainers keep the list of endpoints that Shielded Labs runs outside the repo.

1. Read the `## vN` section of `CHANGELOG.md` for the rollout order.
2. Redeploy each Shielded Labs endpoint in that order with `zeronym/deploy.sh` from the tag commit.
   ```sh
   git switch --detach vN
   ./zeronym/deploy.sh <endpoint>.env
   ```
3. A hub redeploy gives the hub a new Nym address, and every Nym shim must re-assemble. Follow the recovery steps in "If the hub goes down" in the [hub operator guide](zeronym/hub/deploy/caution/OPERATORS.md). Budget well over an hour from start to end.
   1. Wait for `mixnet_connected: true` on `/nym-status`.
   2. Read the new address from `/nym-address`.
   3. Send it to every shim operator.
4. Run `caution verify` on each redeployed endpoint. Confirm that the source commit in its `PROVENANCE` is the tag commit.
5. Point third-party operators at the forum post and the two operator guides. The release owner keeps their contacts outside the repo.
