# Architecture Decision Records

Short, dated records of decisions that a reader of the code would otherwise have
to reverse-engineer. Each one states the context, the decision, what was
measured, and what was rejected — the last two are the parts a diff cannot
carry.

Add a new record as `NNNN-kebab-title.md` (four digits, next free number). Keep
the same section order as `0001`. When a decision is replaced, mark the old
record `Superseded by NNNN` rather than editing its conclusion; the history is
the point.

| ADR | Title | Status |
| --- | ----- | ------ |
| [0001](0001-transport-failure-classification.md) | Classify upstream transport failures by structure, not by message | Accepted |
