# Zero, by Shielded Labs
> Enterprise Zcash Infrastructure

## What is Zero?
Zero is a supported suite of open-source Zcash infrastructure software
maintained by Shielded Labs intended for enterprise use. The goal is to
help exchanges, mining pools, wallet providers, and other ecosystem
partners deploy and operate Zcash infrastructure successfully.

## Why We Are Doing This?
As the ecosystem prepares for Ironwood and future upgrades, operators
need practical support, documentation, migration assistance, and
reliable software options. Zero is intended to provide that support.

## Our Preferred Architecture

For most operators, we generally recommend:

* Zebra for `getblocktemplate`
* Zebra mining nodes
* Zebra in front of existing `zcashd` deployments where appropriate
* Migration toward the Zebra, Zaino, and Zallet (Z3) stack over time

## Why We Are Supporting `zcashd`?
We are not advocating long-term reliance on zcashd.
However, many exchanges, mining pools, and wallet providers currently
depend on zcashd-based infrastructure. Given the aggressive Ironwood
timeline, some partners may not be able to migrate immediately.

For those operators, we intend to provide a supported zcashd fork
with Ironwood support as a practical transition path that includes
a hardcoded end of life date.

## Repository Structure
This is a subtree-based monorepo. Each component (`zcashd/`, `zebra/`,
`zaino/`, `zallet/`) is vendored from its upstream as a git subtree. See
[SUBTREES.md](SUBTREES.md) for the layout and update commands, and
[MAINTENANCE.md](MAINTENANCE.md) for our fork-maintenance policy.

## AI Assistance Disclosure
The Zero monorepo scaffolding, maintenance tooling, and documentation were
developed with the assistance of Claude Code (Anthropic), under human review by
Shielded Labs. Changes vendored from upstream remain the work of their
respective projects.
