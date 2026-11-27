This tool is hidden from the tool list. Think through complex problems by writing structured reasoning. The thought is persisted for session review but is not shown to the user and never appears in visible output.

WHEN TO USE:
- After reading files to plan what to do next
- When choosing between multiple valid approaches
- After test failures to plan diagnosis
- Before making changes to verify sufficiency
- When reflecting on what worked and what did not

WHEN NOT TO USE:
- If the answer is obvious from reading the code
- For action-oriented tasks (just fix the problem directly)
- Trivial cases
- You already know the answer (just act)
- The reasoning is visible in your output

Structure your thoughts:
1. What you now understand (cite file:line)
2. What remains unknown or ambiguous
3. Specific next actions with target files
4. Verification approach

Example:
  thought: "After reading config.rs, the loader is missing validation. Next: add checks in config/load.rs. Verify with cargo test config."
