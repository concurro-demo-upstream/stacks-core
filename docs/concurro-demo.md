# Concurro demonstration

This repository is a development demonstration of Concurro. AI-assisted changes are
explicitly permitted here. This does not change the real Stacks project's policies.

| Role | Repository |
| ---- | ---- |
| Issues and pull requests | `concurro-demo-upstream/stacks-core` |
| Task branches, runner sessions and `.concurro/` | `concurro-demo-fork/stacks-core` |
| Engine and reusable runner workflow | `concurro/concurro` |

## Demo CI

`Concurro Demo CI` runs one `Demo Rust` job on an ordinary `ubuntu-24.04` runner:

```bash
cargo fmt -p stacks-common -- --check
cargo nextest run --locked -p stacks-common --lib --test-threads 4
```

It runs on draft pull requests too, testing GitHub's proposed merge commit. It has a
20-minute timeout and uses no repository secrets, write token, coverage build or
workspace-wide test matrix. Cold dependency compilation still takes several minutes.
Its passing result covers `stacks-common`, not the entire Stacks workspace.

The inherited workflows are preserved, inactive, in `.github/upstream-workflows/`.
Do not restore them or enable the original CI for the demo. There are no automatic
push or scheduled builds, cache uploads or release jobs in the demo workflows.

## Pilot tasks

Start with documentation or a focused `stacks-common` change. Agents may run additional
checks, but must report what actually ran. The target's instructions describe the
representative CI scope; production-wide validation is not implied by demo readiness.

Enroll a fresh issue in the demo upstream with the `concurro` label after the service
and executor configuration are deployed. Concurro publishes a fork branch and an
upstream draft PR, obtains independent Claude and Codex reviews, and marks the PR ready
once those reviews and current CI pass. A human decides whether to merge.

The first fork PR may need a maintainer to approve its workflow run in GitHub. Keep the
default-branch protection rule; no bot bypass is needed. The App needs no access to
`stacks-network/stacks-core` for this demonstration.
