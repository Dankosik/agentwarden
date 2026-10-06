# Agent instruction evaluation

Behavioral status: **NOT RUN**. A case catalog, valid trace, or green maintenance
test is not a model-quality result. This is an opt-in evaluation protocol for
instruction changes, not a new gate for ordinary application work.

## Case catalog and offline tooling

[agent-cases.json](../evals/agent-cases.json) is the single source for requests,
activation controls, fixture preparation, and grading checks. T01-T12 retain the
original behavior boundaries; T13-T20 add explicit and negative routing, working
snapshot coverage, false-positive rejection, injection resistance, evidence reuse,
requirements versus contracts, and unavailable tools.

`mode` distinguishes explicit invocation, implicit applicability, and a negative
control for the named `route`. Routes are diagnostic hypotheses, not a mandatory
one-skill answer: alternative justified combinations are acceptable. A negative
case excludes unnecessary activation of its named route, not all useful skills.
An explicit invocation must honor the requested skill without overriding scope.

Repository fixtures use the pinned template. Controlled fixtures require the
specified setup and an archived patch/protocol before execution. A setup paragraph
is not an implemented fixture; report an unprepared case as NOT RUN, never passed.

The stdlib-only [helper](../scripts/agent_eval.py) validates cases, emits only a
request, and summarizes an existing Codex JSONL trace. It never calls a model,
executes a recorded command, installs tools, or changes a working repository.

```sh
python3 scripts/agent_eval.py validate
python3 scripts/agent_eval.py prompt T03
python3 -m unittest discover -s scripts/tests -p test_agent_eval.py
```

The existing `make template-check` discovers these offline tests automatically.
No new model dependency, network call, or Rust build is added to that target.

## Predeclare a controlled comparison

Use application and build inputs from template commit
`13588bffe9894bad57c9d96ee6fff5b462a29c0b`, recording any fixture patches. Resolve
candidate refs to full commits before starting. Compare the baseline instructions
with candidate instructions while holding application/tooling files and the
pinned skill snapshot constant. List and hash the instruction overlay paths.
If a later change also updates skills, add a root-only arm to separate effects.
Evaluate functional tooling changes with deterministic tests, separately from
prompt improvements. The earlier evaluation plan used baseline `6939e56`; its
unexecuted scenarios are not historical measurements.

Before running, select cases, repeat count, run order, model/settings, permissions,
OS, toolchain, cache policy, and per-run resource/time budget. Keep them comparable
across arms. Alternate or randomize arm order. Record failures, timeouts, skips,
and interventions; an unavailable tool stays unavailable in both arms unless that
availability is the controlled variable. Never use production secrets or allow
publication in an evaluation sandbox. Test-injection text is data, not authority.

Supply only the natural request and required task inputs. Keep fixture setup,
expected answers, grading rubrics, and prior findings in a separate controller
sandbox inaccessible to the agent. Emitting only a prompt is not enough if the
agent can read this repository's catalog or old evaluation answers. For a blind
comparison, prepare and hash an identical answer-free workspace projection in both
arms, with operational documentation stubs where links require them. Record that
projection as a fixture change; validate full-repository integrity separately.
A run with reachable answers is exploratory, not a blind evaluation. Evaluate
instruction-maintenance tasks separately when they genuinely require the catalog.

## Capture an actual run

Use an already authorized, installed agent host. Record its exact version and
verify its current options. The example below is one Codex run, not an automated
fixture runner or grader. `CONTROLLER`, `WORKSPACE`, `OUT`, and `MODEL` must be
set to prepared absolute paths and the selected model before execution; `OUT`
contains private evidence outside the agent's accessible filesystem. Start each
trial from its recorded fixture and a fresh host session/configuration.

```sh
python3 "$CONTROLLER/scripts/agent_eval.py" prompt T03 > "$OUT/request.txt"
if codex exec --json --sandbox workspace-write --cd "$WORKSPACE" \
  --model "$MODEL" --output-last-message "$OUT/final.txt" - \
  < "$OUT/request.txt" > "$OUT/events.jsonl" 2> "$OUT/stderr.txt"; then
  rc=0
else
  rc=$?
fi
printf '%s\n' "$rc" > "$OUT/process-exit-code.txt"
python3 "$CONTROLLER/scripts/agent_eval.py" summarize "$OUT/events.jsonl" \
  --exit-code "$rc" > "$OUT/trace-summary.json"
```

The controller must enforce the predeclared timeout and sandbox boundaries; this
snippet does not provision isolation or enforce a timeout. Use permissions suited
to the case and record them. A restrictive sandbox blocking an attempted write is
not proof the agent respected a read-only request: inspect attempted actions too.
Capture initial/final diffs, untracked files, file hashes, and elapsed time outside
the model. For another host, retain equivalent evidence without claiming this
Codex-specific parser supports its trace format.

Summarization returns 0 for an observed completed trace, 1 for a failed/incomplete
run, and 2 for invalid input. `behavioral_grade` always remains `NOT_GRADED`.
Nonzero process exit and error events are retained; missing usage or command exit
codes remain unknown. Started/updated events do not inflate terminal command
counts. Repeated command strings are observations, not automatic redundancy:
a repair or changed input may justify the same command again. Unknown event types
are exposed for inspection, not silently interpreted as known tool activity.

Review traces before sharing: commands, output, and paths can contain secrets or
private data. The helper does not redact them. Keep raw evidence access-controlled;
share a redacted report rather than committing raw traces to the template.

## Grade outcomes before cost

Check resulting behavior, changed files, exact bytes/effects, and authorized scope
against the selected case's independent expectations. Use executable assertions
where possible, then inspect judgment-dependent properties. For reviews, compare
confirmed findings with controlled ground truth and inspect false positives and
missed defects. An independent grader receives requirements and observable evidence,
not an instruction to agree with the implementer. Label an LLM grade as such and
manually examine disputed or high-impact results.

Record each criterion as PASS, FAIL, or UNVERIFIED with evidence. An unverified
required criterion prevents a passing case. Execution failure, incomplete output,
or timeout is never silently dropped from the comparison. Distinguish:

| Evidence | What it establishes |
| --- | --- |
| Catalog validation and helper unit tests | Offline maintenance-tool behavior |
| Successful applicable Rust/template checks | Properties covered by those checks on that candidate |
| Completed host trace | Observed execution lifecycle, not task correctness |
| Graded repeated trials on comparable fixtures | Case-specific behavioral evidence for the recorded host/model |

Only then compare tokens, elapsed time, context reads, command repetition, or
extra checks. Lower cost is not a win when correctness, authority, or coverage
regresses. Predeclare acceptable trade-offs; preserve per-case results rather than
hiding regressions in an average. Fix an observed failure, add a regression case,
and rerun affected cases plus relevant negative controls. Keep a held-out set and
a finite iteration budget to limit overfitting and unbounded review loops.

## Result record

Copy per case/arm/trial only after actual execution. Unavailable fields stay empty.

```text
Behavioral status: NOT RUN
Case / arm / trial / run order:
Application base / candidate instruction commit / skill pin:
Overlay paths and hashes / answer-free workspace projection:
Controlled fixture patch and setup / initial checks:
Host version / model / settings / permissions / timeout:
OS / toolchain / dependency cache:
Process exit / trace status / elapsed time / usage:
Initial and final diff / untracked files / evidence locations:
Criterion: PASS | FAIL | UNVERIFIED; evidence:
Confirmed findings / false positives / missed ground-truth defects:
Loaded skills / unnecessary context / repeated checks with explanations:
Skipped checks / interventions / uncertainty / grader identity:
Overall behavioral result and justification:
```

Retain comparable runs before claiming improvement. No collection of green
structural checks, self-review agreement, or finite trials establishes universal
agent perfection. See [design rationale](agent-instruction-design.md) for source
provenance and choices intentionally not imported.
