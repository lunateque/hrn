//! `.ipynb` as editable cell text: `# %% [kind] cell:N` followed by the cell source.

use hrn_kernel::{HrnError, Result};
use serde_json::Value;
use std::fmt::Write as _;

/// Render the cells of notebook JSON `text` with their indices.
pub(super) fn convert(text: &str) -> Result<String> {
    let nb: Value = serde_json::from_str(text)
        .map_err(|e| HrnError::Tool(format!("notebook is not valid JSON: {e}")))?;
    let cells = nb
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| HrnError::Tool("notebook has no `cells` array".into()))?;
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        let kind = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("code");
        let source = match cell.get("source") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts.iter().filter_map(Value::as_str).collect(),
            _ => String::new(),
        };
        let _ = writeln!(out, "# %% [{kind}] cell:{i}");
        out.push_str(source.trim_end_matches('\n'));
        out.push_str("\n\n");
    }
    if cells.is_empty() {
        out.push_str("(notebook has no cells)\n");
    }
    Ok(out)
}
