//! Goal mode: the hidden `goal` tool, budget accounting, and the prompt templates that drive
//! omp's autonomous continuation loop.

use hrn_kernel::serde_json::Value;
use hrn_kernel::{
    BoxFuture, Goal, GoalOp, GoalStatus, Result, SessionEffect, Tool, ToolApproval, ToolContext,
    ToolEffect, ToolOutput, ToolSpec,
};
use std::fmt::Write as _;

/// The `goal` description, as data (kept byte-identical to the previous literal).
const DESCRIPTION: &str = include_str!("../prompts/tools/goal.md");
const ACTIVE_TEMPLATE: &str = include_str!("../prompts/goals/goal-mode-active.md");
const CONTINUATION_TEMPLATE: &str = include_str!("../prompts/goals/goal-continuation.md");
const BUDGET_LIMIT_TEMPLATE: &str = include_str!("../prompts/goals/goal-budget-limit.md");

/// Fill omp's `{{placeholders}}` in a goal prompt template.
fn render(template: &str, goal: &Goal) -> String {
    let mut out = String::with_capacity(template.len() + goal.objective.len());
    let mut rest = template;
    while let Some(at) = rest.find("{{") {
        let Some(end) = rest[at..].find("}}") else {
            break;
        };
        let _ = write!(out, "{}", &rest[..at]);
        match &rest[at + 2..at + end] {
            "objective" => out.push_str(&goal.objective),
            "tokensUsed" => {
                let _ = write!(out, "{}", goal.tokens_used);
            }
            "tokenBudget" => match goal.token_budget {
                Some(b) => {
                    let _ = write!(out, "{b}");
                }
                None => out.push_str("none"),
            },
            "remainingTokens" => match goal.remaining_tokens() {
                Some(r) => {
                    let _ = write!(out, "{r}");
                }
                None => out.push_str("unbounded"),
            },
            "timeUsedSeconds" => {
                let _ = write!(out, "{}", goal.time_used_seconds);
            }
            "timeBudget" => match goal.time_budget_secs {
                Some(b) => {
                    let _ = write!(out, "{b} seconds");
                }
                None => out.push_str("none"),
            },
            other => {
                out.push_str("{{");
                out.push_str(other);
                out.push_str("}}");
            }
        }
        rest = &rest[at + end + 2..];
    }
    out.push_str(rest);
    out
}

/// The `goal_context` block carried in the system prompt while a goal is active.
pub fn active_prompt(goal: &Goal) -> String {
    render(ACTIVE_TEMPLATE, goal)
}

/// The hidden continuation steer for the next autonomous turn.
pub fn continuation_prompt(goal: &Goal) -> String {
    render(CONTINUATION_TEMPLATE, goal)
}

/// The wrap-up steer sent once when the token budget flips the goal to `budget-limited`.
pub fn budget_limit_prompt(goal: &Goal) -> String {
    render(BUDGET_LIMIT_TEMPLATE, goal)
}

/// Newlines in a user-supplied objective would close the `<objective>` block early.
fn sanitize_objective(raw: &str) -> String {
    raw.trim().replace(['<', '>'], "")
}

/// Tokens this usage charges to the goal: omp's `input + cacheWrite + output` (cached reads are
/// reused prefix, not new work); hrn's `prompt_tokens` already includes cache reads and writes.
pub fn chargeable_tokens(
    prompt_tokens: u64,
    cache_read: Option<u64>,
    completion_tokens: u64,
) -> u64 {
    prompt_tokens
        .saturating_sub(cache_read.unwrap_or(0))
        .saturating_add(completion_tokens)
}

/// Whether the budget is exhausted (omp flips the goal to `budget-limited` at the boundary).
pub fn budget_exhausted(goal: &Goal, now_ms: i64) -> bool {
    let tokens_over = goal.token_budget.is_some_and(|b| goal.tokens_used >= b);
    let time_over = goal
        .time_budget_secs
        .is_some_and(|b| goal.time_used_seconds.saturating_add(1) >= b);
    let _ = now_ms;
    tokens_over || time_over
}

/// Apply one `goal` op to `goal` (omp's `GoalRuntime` state transitions, reduced to the
/// session's stored goal). Returns `(output text, cleared)`, where `cleared` ends goal mode.
pub fn apply_op(
    goal: &mut Option<Goal>,
    op: GoalOp,
    objective: Option<&str>,
    token_budget: Option<u64>,
    time_budget_secs: Option<u64>,
    now_ms: i64,
) -> std::result::Result<(String, bool), String> {
    match op {
        GoalOp::Create => {
            if goal.is_some() {
                return Err("cannot create a new goal because this session already has one".into());
            }
            let raw = objective.ok_or("objective is required when op=create")?;
            let objective = sanitize_objective(raw);
            if objective.is_empty() {
                return Err("objective is required when op=create".into());
            }
            let created = Goal {
                id: format!("goal_{now_ms}"),
                objective,
                status: GoalStatus::Active,
                token_budget,
                time_budget_secs,
                tokens_used: 0,
                time_used_seconds: 0,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
            };
            let text = format_goal(&created);
            *goal = Some(created);
            Ok((text, false))
        }
        GoalOp::Budget => {
            let g = goal
                .as_mut()
                .ok_or("cannot adjust the budget: no goal on this session")?;
            g.token_budget = token_budget;
            g.time_budget_secs = time_budget_secs;
            g.updated_at_ms = now_ms;
            if matches!(g.status, GoalStatus::BudgetLimited | GoalStatus::Active)
                && !budget_exhausted(g, now_ms)
            {
                g.status = GoalStatus::Active;
            }
            Ok((format_goal(g), false))
        }
        other => {
            let g = goal.as_mut().ok_or("Goal mode is not active.")?;
            match other {
                GoalOp::Get => Ok((format_goal(g), false)),
                GoalOp::Complete => {
                    match g.status {
                        GoalStatus::Complete => return Err("goal is already complete".into()),
                        GoalStatus::Dropped => {
                            return Err("cannot complete a dropped goal".into());
                        }
                        _ => {}
                    }
                    g.status = GoalStatus::Complete;
                    g.updated_at_ms = now_ms;
                    let mut text = format_goal(g);
                    let mut report =
                        String::from("Goal achieved. Report final budget usage to the user: ");
                    if let Some(b) = g.token_budget {
                        let _ = write!(report, "tokens used: {} of {b}", g.tokens_used);
                    }
                    if g.time_used_seconds > 0 {
                        let _ = write!(report, "; time used: {} seconds", g.time_used_seconds);
                    }
                    let _ = writeln!(text, "\n\n{report}.");
                    Ok((text, true))
                }
                GoalOp::Pause => {
                    if matches!(g.status, GoalStatus::Active | GoalStatus::BudgetLimited) {
                        g.status = GoalStatus::Paused;
                    }
                    g.updated_at_ms = now_ms;
                    Ok((format_goal(g), false))
                }
                GoalOp::Resume => {
                    if g.status == GoalStatus::Complete {
                        return Err("goal is already complete".into());
                    }
                    g.status = GoalStatus::Active;
                    g.updated_at_ms = now_ms;
                    if budget_exhausted(g, now_ms) {
                        g.status = GoalStatus::BudgetLimited;
                    }
                    Ok((format_goal(g), false))
                }
                GoalOp::Drop => {
                    g.status = GoalStatus::Dropped;
                    g.updated_at_ms = now_ms;
                    Ok(("No active goal.".to_owned(), true))
                }
                GoalOp::Create | GoalOp::Budget => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

/// The model/user-facing rendering of one goal (omp's goal tool output).
pub fn format_goal(goal: &Goal) -> String {
    let mut text = format!(
        "Goal: {}\nStatus: {}\nTokens: {} used",
        goal.objective,
        goal.status.as_str(),
        goal.tokens_used
    );
    if let Some(b) = goal.token_budget {
        let _ = write!(text, " / {b} budget");
    }
    if let Some(r) = goal.remaining_tokens() {
        let _ = write!(text, "\nRemaining tokens: {r}");
    }
    let _ = write!(text, "\nTime used: {} seconds", goal.time_used_seconds);
    if let Some(b) = goal.time_budget_secs {
        let _ = write!(text, " / {b} budget");
    }
    text
}

/// The hidden `goal` tool: names an op, the runtime applies it to the session's goal.
pub struct GoalTool;

impl Tool for GoalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "goal".to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {"enum": ["create", "get", "complete", "pause", "resume", "drop", "budget"]},
                    "objective": {"type": "string", "description": "goal objective (op=create)"},
                    "token_budget": {"type": "integer", "description": "positive token budget (op=create|budget)"},
                    "time_budget_secs": {"type": "integer", "description": "positive seconds budget (op=create|budget)"}
                },
                "required": ["op"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn approval(&self, _args: &Value) -> ToolApproval {
        ToolApproval::read()
    }

    fn hidden(&self) -> bool {
        true
    }

    fn execute(&self, _ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let out = match args.get("op").and_then(Value::as_str).map(parse_op) {
            None | Some(Err(())) => ToolOutput::err(
                "goal: `op` must be one of create, get, complete, pause, resume, drop, budget",
            ),
            Some(Ok(op)) => {
                let objective = args
                    .get("objective")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let token_budget = positive(&args, "token_budget");
                let time_budget = positive(&args, "time_budget_secs");
                let mut out = ToolOutput::ok("(goal update pending)");
                out.session_effect = Some(SessionEffect::Goal {
                    op,
                    objective,
                    token_budget,
                    time_budget_secs: time_budget,
                });
                out
            }
        };
        Box::pin(async move { Ok(out) })
    }
}

fn parse_op(raw: &str) -> std::result::Result<GoalOp, ()> {
    Ok(match raw {
        "create" => GoalOp::Create,
        "get" => GoalOp::Get,
        "complete" => GoalOp::Complete,
        "pause" => GoalOp::Pause,
        "resume" => GoalOp::Resume,
        "drop" => GoalOp::Drop,
        "budget" => GoalOp::Budget,
        _ => return Err(()),
    })
}

/// A positive integer argument, or `None` for absent/zero/negative (an explicit 0 clears the
/// budget, which the model spells by omitting it; `null` also clears).
fn positive(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_lifecycle_transitions() {
        let mut goal: Option<Goal> = None;
        // create needs an objective and refuses duplicates.
        assert!(apply_op(&mut goal, GoalOp::Create, None, None, None, 100).is_err());
        let (text, _) = apply_op(
            &mut goal,
            GoalOp::Create,
            Some("fix the loader"),
            Some(1000),
            None,
            100,
        )
        .unwrap();
        assert!(text.contains("fix the loader"));
        assert!(apply_op(&mut goal, GoalOp::Create, Some("x"), None, None, 200).is_err());
        // drop ends goal mode; the runtime (apply_goal_effects) clears the slot.
        let (_, cleared) = apply_op(&mut goal, GoalOp::Drop, None, None, None, 200).unwrap();
        assert!(cleared);
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Dropped);
        goal = None;
        // budget-limited at the boundary, resumable after raising it.
        apply_op(&mut goal, GoalOp::Create, Some("go"), Some(10), None, 300).unwrap();
        let created = goal.as_mut().unwrap();
        created.tokens_used = 10;
        assert!(budget_exhausted(created, 300));
        apply_op(&mut goal, GoalOp::Budget, None, Some(100), None, 400).unwrap();
        assert_eq!(goal.as_ref().unwrap().status, GoalStatus::Active);
        // complete reports usage and ends the loop.
        let (text, cleared) = apply_op(&mut goal, GoalOp::Complete, None, None, None, 500).unwrap();
        assert!(cleared && text.contains("Goal achieved"));
    }
}
