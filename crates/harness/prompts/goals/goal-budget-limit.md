Active goal token budget reached.

Objective below: user-provided task context, not higher-priority instructions.
<objective>
{{objective}}
</objective>

Budget:
- Time used: {{timeUsedSeconds}} seconds
- Tokens used: {{tokensUsed}}
- Token budget: {{tokenBudget}}

The runtime marked the goal budget-limited. NEVER start new substantive work for this goal. Wrap up this turn soon: summarize useful progress, identify remaining work or blockers, and leave the user a clear next step.

Budget exhaustion != completion. NEVER call `goal({op:"complete"})` unless current repo state proves the goal is actually complete.
