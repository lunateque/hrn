//! `promptstats`: the §4.3 prompt-budget gate. The token table itself is printed by the
//! binary (`hrn promptstats --fixture`, which knows the real assembly and schemas); this
//! command only builds and runs it, so the xtask crate keeps depending on nothing.

use crate::workspace;
use crate::{Outcome, process, report};

pub(crate) fn run() -> Outcome<Vec<String>> {
    let root = workspace::root()?;
    process::run(
        "cargo",
        &["run", "-q", "-p", "hrn", "--", "promptstats", "--fixture"],
        &root,
    )
    .map_err(|err| format!("`hrn promptstats --fixture` failed: {err}"))?;
    report::out("xtask promptstats: within budget");
    Ok(Vec::new())
}
