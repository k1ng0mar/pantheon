# Decision record and evidence ledger

Two files make a run reusable. The Chairperson maintains both. This is bookkeeping, not argument, so it does not breach separation of powers.

## `.board/EVIDENCE.md`

One row per claim that a source supports.

| Claim | Source | Confidence | Reported by |
| --- | --- | --- | --- |
| ... | URL | high / medium / low | persona name |

Rules:

- No source, no row. An unsourced claim stays in the agent's own dossier marked `[UNVERIFIED]`.
- Confidence is the reporter's, recorded at the time. Do not upgrade it later without a new source.
- The report's Sources section is generated from this file, not retyped.

## `.board/DECISIONS.md`

One block per candidate, appended as decisions are made.

```
### idea-slug

- Stage: scouts / validators / realists / lab
- Decision: PURSUE / KILL / HOLD
- Reason: one sentence
- Kill criteria and thresholds: if any
- Review trigger: the new evidence that would reopen this
- Owner: the user, or nobody yet
```

Rules:

- A killed idea gets an entry even when it dies in the lab.
- HOLD means the evidence is insufficient. It does not mean the idea is good.
- A later run reads this file first. It does not re-litigate a kill unless the review trigger fired.
- The Chairperson records the decision when it is made, not at the end of the run.
