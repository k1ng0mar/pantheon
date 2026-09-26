# Pipelines

The orchestration pipeline runs a task through six fixed stages with human
checkpoints. It exists for MVP-style work where you want the model to do
the whole loop but you want to see and approve the plan and the review
before they count.

## Stages

```
intake -> research -> plan -> [GATE] -> implement -> review -> [GATE] -> commit
```

| Stage | Prompt shape | Output becomes |
|---|---|---|
| intake | "Restate the task as a precise, testable specification" | input to research |
| research | "Gather context and constraints; facts, unknowns, risks" | input to plan |
| plan | "Produce a step-by-step implementation plan" | gated, then input to implement |
| implement | "Execute this plan" (may loop with the evaluator) | input to review |
| review | "Review against the plan; verdict and reasons" | gated, then input to commit |
| commit | "Produce the commit message and summary" | final output |

Each stage's output is the next stage's input, so the chain carries
context forward without re-reading the original spec.

## Durability

Every stage is a durable operation (`pipeline.stage`) in the operations
store, same state machine as tool work: `translate -> execute ->
translate_result`, each phase persisted before the next side effect. If
the process dies mid-pipeline:

- Completed stages are skipped on resume (their output is in SQLite).
- The stage that was mid-flight resumes at its persisted phase.
- Gates remember their decision; an approved gate does not re-ask.

You resume by running the same command again. There is no separate
"resume pipeline" verb.

## Gates

After plan and after review, the pipeline parks:

```
run id: mybuild
pipeline parked: approve or deny scope mybuild:plan to continue
approve with: pantheon pipeline mybuild --approve plan
deny with:    pantheon pipeline mybuild --deny plan
```

- Approve: the gate settles `completed`, the decision text flows into the
  next stage, and the next invocation continues from there.
- Deny: the gate settles `failed`, and the next invocation reports
  `PIPELINE_DENIED`. The pipeline stops by design; the human said no.

Gates are operation rows (`pipeline.gate`) with the decision in their
state, so approval survives restarts and is auditable through the same
operations list as everything else.

## The generator/evaluator loop

Inside `implement`, each iteration runs the executor, then asks the
evaluator whether the output is acceptable. A rejection makes the output
itself the next iteration's input (the model sees its own work plus an
implicit "do better"). Defaults:

- Evaluator: accept-all (the review gate still applies). Enable the
  strict evaluator with `PANTHEON_PIPELINE_EVAL=1`, in which case a second
  session is asked to answer ACCEPT or REJECT.
- Iterations: 3. Non-convergence fails with `PIPELINE_EVAL_LOOP`.

Each iteration is its own durable operation (`<run>:implement#N`), so a
crash mid-loop resumes at the right iteration.

## CLI

```sh
pantheon pipeline --spec "build a login page"            # new pipeline
pantheon pipeline mybuild --spec "build a login page"    # named run id
pantheon pipeline mybuild --approve plan
pantheon pipeline mybuild --deny review
pantheon pipeline mybuild --spec "build a login page"    # resume
```

Printed output is `== stage ==` sections with each stage's text. The run
also appears in the ledger (`pantheon logs mybuild`).

## What a stage executor is

Stages do not run inline; the runner drives a `StageExecutor` trait. The
CLI wires a runtime executor (one session per stage, stage-specific prompt
framing). Tests use a scripted executor. This seam is what keeps the
pipeline logic testable without a model and makes "use an external coding
agent as the implement stage" a straightforward adapter later.
