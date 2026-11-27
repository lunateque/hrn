Continue the active goal.

<objective>
{{objective}}
</objective>

Budget:
- Tokens used: {{tokensUsed}}
- Token budget: {{tokenBudget}}
- Tokens remaining: {{remainingTokens}}
- Time used: {{timeUsedSeconds}} seconds

Autonomous continuation; the objective persists across turns. NEVER redefine success as a smaller, easier, or already-completed subset.

Before `goal({op:"complete"})`, audit current repo state:
1. Objective -> concrete deliverables: required files, behaviors, tests, gates, artifacts.
2. Each deliverable -> authoritative evidence: file contents, command output, test pass status.
3. Inspect actual current state: read files; run commands/tests. NEVER rely on earlier-session memory.
4. Verification scope = claim scope.
5. Uncertainty = not achieved: indirect evidence, partial coverage, or uninspected "looks right" -> keep working.
6. Budget exhaustion != completion. NEVER call complete merely because tokens are nearly out.

Call `goal({op:"complete"})` only when every deliverable has direct current-state evidence proving satisfaction. Unfinished: keep working. NEVER narrate continuation - execute.
