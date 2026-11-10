# Concurro demonstration

This repository is a development demonstration of Concurro. AI-assisted changes are
explicitly permitted here. This does not change the real Stacks project's policies.

| Role | Repository |
| ---- | ---- |
| Issues and pull requests | `concurro-demo-upstream/stacks-core` |
| Task branches, runner sessions and `.concurro/` | `concurro-demo-fork/stacks-core` |
| Engine and reusable runner workflow | `concurro/concurro` |

## Demo CI

`Concurro Demo CI` runs one `Demo Rust` job on an `ubuntu-24.04` runner, including
on draft pull requests. It tests GitHub's proposed merge commit with a 30-minute
job timeout, four build jobs and four test threads. Incremental compilation is disabled.

The shared [build action](../.github/actions/concurro-demo-rust/action.yml) is the
source of truth for the test selection. A single Cargo invocation builds the four
packages together, with `stacks-common/rusqlite` enabled:

| Package | Library tests |
| ---- | ---- |
| `stacks-common` | All |
| `stacks-marf` | All |
| `libsigner` | All |
| `stackslib` | Clarity historical/forking and ephemeral replay; HTTP parsing and routing; account, map-entry, data-variable, simulation, replay, read-only-call, transaction-submission and node-info RPCs; Clarity snapshot storage; core/mempool; block and transaction validation |

Formatting is checked for all four packages. Ignored tests remain ignored. This is
focused coverage, not the full workspace, live Bitcoin integration, signer-system
integration, benchmarks or production-wide validation. New pilots need their test
areas included before enrollment.

PR jobs only restore caches. A nightly main-branch job (02:23 UTC), also available
through `workflow_dispatch` on main, builds the same selection with `--no-run` and
saves the trusted cache; it does not run tests. Cache keys include manifests, the
lockfile, toolchain, Cargo configuration, build action and workflow. Changes to the
selection therefore start a new cache family.

Neither job uses repository secrets or write tokens. Only the trusted main-branch
warmer saves caches. The inherited workflows remain inactive in
`.github/upstream-workflows/`; they are not part of demo CI.

## Pilot tasks

Choose a focused task whose relevant tests are covered by demo CI. Agents may run
additional checks, but must report what actually ran. The target's instructions describe the
representative CI scope; production-wide validation is not implied by demo readiness.

Enroll a fresh issue in the demo upstream with the `concurro` label after the service
and executor configuration are deployed. Concurro publishes a fork branch and an
upstream draft PR, obtains independent Claude and Codex reviews, and marks the PR ready
once those reviews and current CI pass. A human decides whether to merge.

The first fork PR may need a maintainer to approve its workflow run in GitHub. Keep the
default-branch protection rule; no bot bypass is needed. The App needs no access to
`stacks-network/stacks-core` for this demonstration.
