# Agent instruction design decisions

Reviewed on 2026-09-16 against template commit
`13588bffe9894bad57c9d96ee6fff5b462a29c0b`. This is dated provenance for instruction
maintenance, not startup context or a claim of measured model improvement.
[AGENTS.md](../AGENTS.md) remains the policy owner; vendored skill bytes, their
source pin, application code, dependencies, and CI policy are unchanged.

## Adaptations and limits

| Source | Adopted here | Deliberately not imported |
| --- | --- | --- |
| Matt Pocock: writing for agents | Conditional context pointers, observable completion, one owner per rule, on-demand references | A copied writing framework in every task; arbitrary prose or line-count gates |
| Matt Pocock: code review | Separate requirements and standards/contracts questions, fixed scope, independent review when tools exist | Mandatory tracker setup or a new spec; automatic defects from subjective code smells; simulated subagents |
| Matt Pocock: TDD | Behavioral boundaries, independent expectations, small vertical slices, reproduction before repair where feasible | User approval before every test; forced TDD for documentation or read-only work |
| Alibaba OpenCodeReview | Complete file accounting, related-file grouping, relevant rules, current anchors, separate falsification of findings | Installing OCR or its runtime; claiming language instructions provide its deterministic guarantees or benchmark results |
| OpenAI: evaluating skills | Natural requests, explicit/implicit/negative controls, traces, outcome checks before cost, repeatable comparisons | Keyword matching as a behavioral pass; paid model runs in normal CI; invented token or latency improvements |
| OpenAI: rethinking skills and prompts | Compact root policy, selective activation, completion through repair, autonomy within authority | Model-name-specific commands or permissions; a mandatory long workflow on small changes |
| Thariq / Anthropic: context engineering | Project-specific root guidance; review on demand; typed tool interfaces; executable expectations and grading criteria as references | Repeated recipes across instruction surfaces; blind deletion of safety boundaries; vendor benchmark claims as local results |

The original root already had selective reading, small verification boundaries,
offline pinned skills, and honest unavailable-evidence rules. Those are preserved,
not presented as newly invented. Added mechanisms address review scope/precision
and evaluation observability. The review document is local guidance rather than
a seventeenth vendored skill; this preserves the upstream integrity contract.

The Thariq guidance reinforces the same architecture rather than adding another
mandatory workflow: the root routes to the review reference; the helper exposes
explicit commands and statuses; the catalog defines observable expectations.
Host-managed memory is not replaced by appending transient observations to AGENTS.md.

A real review uses tools to select a snapshot and inspect evidence. The review
instructions cannot enforce file coverage or prevent malicious tool execution by
themselves. Likewise, the evaluation helper validates a catalog and summarizes
traces; it does not create all controlled fixtures, run models, or certify behavior.
The [evaluation protocol](agent-evaluation.md) states those boundaries explicitly.

## Source records

The GitHub files below were read as references; their implementation and prose
were not vendored. File blob identities identify the inspected contents even if
the linked default branch later moves.

- [Writing for agents](https://github.com/mattpocock/skills/blob/main/skills/productivity/writing-for-agents/SKILL.md): Git blob `a37608daf6e835e767deecfb498facecaaba82ba`.
- [Code review](https://github.com/mattpocock/skills/blob/main/skills/engineering/code-review/SKILL.md): Git blob `e28d7acbf7b3bb4d7817b7eb5d9c105af03f6ec4`.
- [TDD](https://github.com/mattpocock/skills/blob/main/skills/engineering/tdd/SKILL.md): Git blob `8fc086710806190ee7c4baa32089cb877a75736a`.
- [OpenCodeReview tools](https://github.com/alibaba/open-code-review/blob/f1101fd7f51304c82e4a4f292bbee88aea0823cf/pages/src/content/docs/en/tools.md): commit `f1101fd7f51304c82e4a4f292bbee88aea0823cf`; [project overview](https://github.com/alibaba/open-code-review) also inspected. The supplied repository URL had an extra final Cyrillic character; the existing canonical repository was used.
- [Evaluating skills](https://developers.openai.com/blog/eval-skills) and [Rethinking skills and prompts](https://developers.openai.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra): retrieved 2026-09-16; web pages are not immutable source pins.
- [Codex CLI reference](https://developers.openai.com/codex/cli/reference/) and [non-interactive mode](https://learn.chatgpt.com/docs/non-interactive-mode): checked for the example's JSONL and sandbox options on the same date. Verify installed-host compatibility when running.
- [Requested X article](https://x.com/trq212/article/2080710971228918066): direct X text remained inaccessible. Instead, the author's [official Claude blog publication](https://claude.com/blog/the-new-rules-of-context-engineering-for-claude-5-generation-models), by Thariq Shihipar, dated 2026-07-24, was inspected on 2026-09-16. Attribution here is to that accessible primary publication, not an unverified X transcript.

Changing this record does not authorize a skill refresh, external installation,
publication, or additional user workflow. Behavioral evaluation remains NOT RUN
until actual case results and their evidence have been recorded.
