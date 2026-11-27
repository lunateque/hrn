//! The `ast_grep` tool: structural search, executed by the `hrn-ast` sidecar.

use crate::{Config, check_scope, tags_of};
use hrn_edge_ast::protocol::{Data, Request};
use hrn_edge_ast::{show_search, table};
use hrn_edge_tools::args::{bounded_usize, opt_str, required_str, summary_of};
use hrn_kernel::serde_json::{Value, json};
use hrn_kernel::{
    BoxFuture, HrnError, Result, Tool, ToolContext, ToolEffect, ToolOutput, ToolSpec,
};

/// Structural code search with AST patterns.
pub(crate) struct AstGrepTool {
    cfg: Config,
}

impl AstGrepTool {
    pub(crate) const fn new(cfg: Config) -> Self {
        Self { cfg }
    }
}

impl Tool for AstGrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ast_grep".to_owned(),
            description: crate::DESCRIPTION_GREP.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pat": { "type": "string", "description": "One AST pattern." },
                    "path": {
                        "type": "string",
                        "description": "File, directory or glob; several as a `;`-separated list \
                                        (`\"src; tests\"`). Omitted -> the workspace root."
                    },
                    "lang": {
                        "type": "string",
                        "description": format!("Language override for every file ({}).", table::aliases())
                    },
                    "skip": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Matches to skip when paging (default 0)."
                    }
                },
                "required": ["pat"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }

    fn summarize_call(&self, args: &Value) -> String {
        summary_of(args, "pat")
    }

    fn execute(&self, ctx: &ToolContext, args: Value) -> BoxFuture<Result<ToolOutput>> {
        let cwd = ctx.cwd.clone();
        let cancel = ctx.cancel.clone();
        let limit = self.cfg.max_matches;
        Box::pin(async move {
            let pattern = required_str(&args, "pat", "ast_grep")?.trim().to_owned();
            if pattern.is_empty() {
                return Ok(ToolOutput::err("ast_grep: `pat` must not be empty"));
            }
            let lang = match opt_str(&args, "lang")
                .map(str::trim)
                .filter(|l| !l.is_empty())
            {
                None => None,
                Some(raw) => {
                    if table::key_of(raw).is_none() {
                        return Ok(ToolOutput::err(format!(
                            "ast_grep: unsupported language `{raw}` ({})",
                            table::aliases()
                        )));
                    }
                    Some(raw.to_owned())
                }
            };
            let raw_path = opt_str(&args, "path").unwrap_or(".").to_owned();
            let skip = bounded_usize(&args, "skip", 0, 0, usize::MAX / 2);
            let directory = check_scope(&cwd, &raw_path, "`path` entries must not be empty")?;
            let request = Request::Search {
                cwd: cwd.to_string_lossy().into_owned(),
                path: raw_path,
                lang,
                pattern,
                skip,
                limit,
            };
            let response = crate::client::run(&request, &cancel).await?;
            let data = response.into_data().map_err(hrn_kernel::HrnError::Tool)?;
            let found = match data {
                Data::Search(found) => found,
                Data::Rewrite(_) | Data::Syntax(_) => {
                    return Err(HrnError::Internal(
                        "ast: the sidecar answered a search with an unexpected payload".to_owned(),
                    ));
                }
            };
            let tag = tags_of(&cwd);
            Ok(ToolOutput::ok(show_search(&found, directory, &tag).text))
        })
    }
}
