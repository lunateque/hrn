Manage the active goal-mode objective.
Single `op` field:
- `create`: starts goal mode; requires `objective`, optional positive `token_budget` / `time_budget_secs`. Only when no goal exists.
- `get`: current goal and remaining budget.
- `complete`: marks the goal complete, only when actually done and every deliverable is verified against current evidence. NEVER because the budget is low or the turn is ending.
- `pause`: stop continuation while blocked on the user.
- `resume`: re-activate a paused goal.
- `drop`: discard the goal and end goal mode.
- `budget`: replace `token_budget` / `time_budget_secs` (null clears one).
