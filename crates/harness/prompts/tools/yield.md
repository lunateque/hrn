This tool is hidden from the tool list. Return structured data as a subagent.

WHEN TO USE:
- You are a subagent and the prompt specifies a JSON schema for your result
- The answer is complete and ready to return

WHEN NOT TO USE:
- You are the main agent - yield is only for subagents (it has no effect there)
- The task is not done
- No structured output was requested

Post-completion: after calling yield, STOP - do not make more tool calls. One invocation per turn.
Note: yield bypasses the requirement that includes files touched, commands run, and verification.
