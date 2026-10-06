# git.agentic

Shared language for the git.agentic project — "Git for agent behavior".
Terms land here as sessions resolve them; definitions carry no implementation detail.

## Language

### Decision records

**Proposed / Accepted (ADR `Status:`)**:
The state of the *decision*, never of the implementation.
An ADR whose control has already shipped may still read Proposed only if the decision itself was never ratified.
_Avoid_: Implemented, Shipped, Done (as `Status:` values)

**Closed in**:
The implementation record on an ADR that `Closes:` a threat-model row —
the commit or PR (with date) where the control actually landed.
The threat-model row points back at the ADR.
_Avoid_: fixed in, resolved by

### Performance claims

**Commitment**:
A §9 performance figure that has been verified by measurement at the committed shape.
_Avoid_: aspiration, goal

**Target under verification**:
A §9 figure adopted as binding but not yet measured at the committed shape.
Calling one of these a commitment is doc-truth drift.
_Avoid_: commitment (until verified)
