---
applyTo: ".github/workflows/**,.githooks/**"
---

# Reviewing CI workflows

These workflows build and publish release artifacts and container images, and
several hold `packages: write`. A weakness here compromises what users
download, so treat workflow changes as supply-chain changes.

## Injection and privilege

- **Never interpolate untrusted input into a `run:` block.** `${{
  github.event.pull_request.title }}`, branch names, and comment bodies are
  attacker-controlled on a public repository and become shell code when
  interpolated. They belong in `env:` and then referenced as shell variables.
- **`pull_request_target` and `workflow_run` run with write permissions and
  repository secrets.** Any change that gives one of these access to pull
  request head code is a privilege escalation. Flag it.
- Flag any widening of `permissions:`, and any job that gets a token scope it
  does not use.
- Actions pinned by tag can move under you. Prefer a commit SHA for anything
  running in a job that holds write permissions.

## Silent failure

A workflow that does not run is indistinguishable from one that passed. This
repository has been bitten by exactly that: a pull request matched the
`paths:` filters, no check suite was created at all, and it merged with no CI
and nothing on the page saying so.

- Be suspicious of `paths:` filters, `continue-on-error`, `if:` conditions that
  can skip a whole job, and `|| true` in a verification step.
- A gate that decides whether to run expensive work should still report a
  result either way, so a skipped run is visible rather than absent.
- Verification steps must fail loudly. If a check errors, that is not a pass.

## Reproducibility

Release and image builds back a claim that a published artifact matches its
source. Flag caching that could serve a stale layer into a build under test,
and anything that makes output depend on when or where it ran.
