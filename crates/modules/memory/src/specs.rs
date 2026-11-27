//! Model-facing schemas of the memory tools.

use hrn_kernel::ToolSpec;
use hrn_kernel::serde_json::{Value, json};

/// Tool names.
pub(crate) const RETAIN: &str = "retain";
pub(crate) const RECALL: &str = "recall";
pub(crate) const REFLECT: &str = "reflect";
pub(crate) const MEMORY_EDIT: &str = "memory_edit";
pub(crate) const LEARN: &str = "learn";

/// The tool descriptions, as data.
const DESCRIPTION_RETAIN: &str = include_str!("../prompts/retain.md");
const DESCRIPTION_RECALL: &str = include_str!("../prompts/recall.md");
const DESCRIPTION_REFLECT: &str = include_str!("../prompts/reflect.md");
const DESCRIPTION_MEMORY_EDIT: &str = include_str!("../prompts/memory_edit.md");
const DESCRIPTION_LEARN: &str = include_str!("../prompts/learn.md");

fn scope_schema() -> Value {
    json!({
        "type": "string", "enum": ["project", "global"],
        "description": "`project` (default): this repository only; `global`: every project."
    })
}

pub(crate) fn retain() -> ToolSpec {
    ToolSpec {
        name: RETAIN.to_owned(),
        description: DESCRIPTION_RETAIN.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array", "minItems": 1,
                    "description": "One or more memories to store.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "The self-contained fact." },
                            "context": { "type": "string", "description": "Optional provenance: where or why this was learned." },
                            "scope": scope_schema()
                        },
                        "required": ["content"]
                    }
                }
            },
            "required": ["items"]
        }),
    }
}

pub(crate) fn recall() -> ToolSpec {
    ToolSpec {
        name: RECALL.to_owned(),
        description: DESCRIPTION_RECALL.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language search query; distinctive keywords work best." }
            },
            "required": ["query"]
        }),
    }
}

pub(crate) fn reflect() -> ToolSpec {
    ToolSpec {
        name: REFLECT.to_owned(),
        description: DESCRIPTION_REFLECT.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The question to answer from memory." },
                "context": { "type": "string", "description": "Extra guidance that narrows what is relevant." }
            },
            "required": ["query"]
        }),
    }
}

pub(crate) fn memory_edit() -> ToolSpec {
    ToolSpec {
        name: MEMORY_EDIT.to_owned(),
        description: DESCRIPTION_MEMORY_EDIT.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["update", "forget", "invalidate"] },
                "id": { "type": "string", "description": "Memory id returned by `recall`." },
                "content": { "type": "string", "description": "Replacement text for `update`." },
                "importance": { "type": "number", "description": "Replacement importance for `update`, clamped to 0..1." },
                "replacement_id": { "type": "string", "description": "For `invalidate`: the id of the memory that supersedes this one." }
            },
            "required": ["op", "id"]
        }),
    }
}

pub(crate) fn learn() -> ToolSpec {
    ToolSpec {
        name: LEARN.to_owned(),
        description: DESCRIPTION_LEARN.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "memory": { "type": "string", "description": "The durable, self-contained lesson." },
                "context": { "type": "string", "description": "Source context for the lesson." },
                "scope": scope_schema()
            },
            "required": ["memory"]
        }),
    }
}
