## What to look for

Report defects: places where the changed code does something other than what it
is evidently meant to do. A security consequence is welcome but not required.
Weight your attention in this order.

1. **Correctness.** Off-by-one and boundary errors, inverted conditions, wrong
   variable used, integer overflow and truncation, sign confusion, wrong
   arithmetic on amounts or heights, comparisons that mix units.
2. **Control flow.** Early returns that skip required cleanup, `break` versus
   `continue` mixups, unreachable branches, loops whose termination depends on
   a value the body changes, a missing `default` or `else` where every case
   matters.
3. **State and lifetime.** Use-after-free, use-after-move, dangling references,
   iterator or reference invalidation after a container mutates, objects read
   before they are fully initialized, resources leaked on an error path.
4. **Concurrency.** Data races on shared state, a lock held across a call that
   can block or re-enter, a lock released between a check and the action it
   guards, deadlock from inconsistent lock ordering.
5. **Error handling.** A dropped status or `Result`, an exception path that
   leaves state half-updated, an error swallowed so the caller cannot tell
   failure from empty success, retries on something not idempotent.
6. **API misuse.** A library called with the wrong preconditions, a contract
   documented in the callee that the new caller violates, a changed function
   whose remaining callers were not updated.
7. **Tests.** A behaviour change with no test covering it, a test that passes
   whether or not the fix is present, a test weakened or disabled by this
   change.

## Efficiency

Report a cost only when it is real in a path this change runs:

- Accidental O(n^2) over a collection whose size the network controls.
- A copy of a large structure inside a hot loop.
- A recomputation whose result was already available.

Do not report micro-optimizations, and do not restructure working code for
taste. Report duplicated logic only when the copies can drift apart and give
different answers.

## Severity

- **High** — wrong consensus, wrong money, data loss, corruption, or a crash
  reachable in ordinary operation.
- **Medium** — wrong results in a reachable but narrower case, a resource leak
  that accumulates, a broken error path.
- **Low** — a real defect with bounded consequences: wasted work, a misleading
  error message, a test that does not test what it claims.

Do not inflate a Low finding into a Medium one. Saying a slip is harmless is
more useful than implying a risk that is not there.

## Do not report

Formatting, naming, comment wording, import order, or anything a linter or
formatter enforces. If the only thing wrong with a line is that you would have
written it differently, say nothing.
