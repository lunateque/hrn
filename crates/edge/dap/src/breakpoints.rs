//! Breakpoints of a session: the desired sets live here and are replayed to every live target,
//! because each `set*Breakpoints` request replaces the adapter's whole list for its kind.

use crate::session::Session;
use crate::state::Status;
use hrn_kernel::Result;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::client::DapClient;

/// One breakpoint of any kind; the fields that identify its kind are set.
#[derive(Clone, Debug, Default)]
pub struct Breakpoint {
    /// Source breakpoints: the line.
    pub line: Option<u64>,
    /// Function breakpoints: the name.
    pub name: Option<String>,
    /// Instruction breakpoints: the reference.
    pub reference: Option<String>,
    /// Instruction breakpoints: the offset.
    pub offset: Option<i64>,
    /// Data breakpoints: the data id.
    pub data_id: Option<String>,
    /// Data breakpoints: `read`, `write` or `readWrite`.
    pub access_type: Option<String>,
    /// Stop only when this holds.
    pub condition: Option<String>,
    /// Stop only after this many hits.
    pub hit_condition: Option<String>,
    /// The adapter's id for it.
    pub id: Option<i64>,
    /// Whether the adapter bound it.
    pub verified: bool,
    /// The adapter's remark (why it is not verified).
    pub message: Option<String>,
}

/// Which list a request edits.
#[derive(Clone, Debug)]
pub enum Kind {
    /// Source breakpoints of one file (absolute path).
    Source(String),
    /// Function breakpoints.
    Function,
    /// Instruction breakpoints.
    Instruction,
    /// Data breakpoints.
    Data,
}

/// The desired breakpoints of a session.
#[derive(Default)]
pub(crate) struct Breakpoints {
    source: BTreeMap<String, Vec<Breakpoint>>,
    function: Vec<Breakpoint>,
    instruction: Vec<Breakpoint>,
    data: Vec<Breakpoint>,
}

/// The edited list plus what failed on the other targets.
#[derive(Clone, Debug)]
pub struct BreakpointResult {
    /// The list as the focused adapter now holds it.
    pub breakpoints: Vec<Breakpoint>,
    /// Targets that did not take the same list.
    pub warnings: Vec<String>,
}

impl Breakpoints {
    fn list(&self, kind: &Kind) -> Vec<Breakpoint> {
        match kind {
            Kind::Source(path) => self.source.get(path).cloned().unwrap_or_default(),
            Kind::Function => self.function.clone(),
            Kind::Instruction => self.instruction.clone(),
            Kind::Data => self.data.clone(),
        }
    }

    fn store(&mut self, kind: &Kind, list: Vec<Breakpoint>) {
        match kind {
            Kind::Source(path) if list.is_empty() => {
                self.source.remove(path);
            }
            Kind::Source(path) => {
                self.source.insert(path.clone(), list);
            }
            Kind::Function => self.function = list,
            Kind::Instruction => self.instruction = list,
            Kind::Data => self.data = list,
        }
    }
}

fn set(map: &mut Map<String, Value>, key: &str, value: Value) {
    map.insert(key.to_owned(), value);
}

fn set_str(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(v) = value.filter(|v| !v.is_empty()) {
        set(map, key, json!(v));
    }
}

/// The DAP request that makes an adapter hold exactly `list` for `kind`.
fn request_for(kind: &Kind, list: &[Breakpoint]) -> (&'static str, Value) {
    let entries: Vec<Value> = list
        .iter()
        .map(|b| {
            let mut e = Map::new();
            if let Some(line) = b.line {
                set(&mut e, "line", json!(line));
            }
            set_str(&mut e, "name", b.name.as_deref());
            set_str(&mut e, "instructionReference", b.reference.as_deref());
            if let Some(offset) = b.offset {
                set(&mut e, "offset", json!(offset));
            }
            set_str(&mut e, "dataId", b.data_id.as_deref());
            set_str(&mut e, "accessType", b.access_type.as_deref());
            set_str(&mut e, "condition", b.condition.as_deref());
            set_str(&mut e, "hitCondition", b.hit_condition.as_deref());
            Value::Object(e)
        })
        .collect();
    match kind {
        Kind::Source(path) => {
            let name = Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            (
                "setBreakpoints",
                json!({"source": {"path": path, "name": name}, "breakpoints": entries}),
            )
        }
        Kind::Function => ("setFunctionBreakpoints", json!({"breakpoints": entries})),
        Kind::Instruction => ("setInstructionBreakpoints", json!({"breakpoints": entries})),
        Kind::Data => ("setDataBreakpoints", json!({"breakpoints": entries})),
    }
}

fn apply_response(list: &mut [Breakpoint], response: &Value) {
    let answers = response.get("breakpoints").and_then(Value::as_array);
    for (index, bp) in list.iter_mut().enumerate() {
        let answer = answers.and_then(|a| a.get(index));
        bp.id = answer.and_then(|a| a.get("id")).and_then(Value::as_i64);
        bp.verified = answer
            .and_then(|a| a.get("verified"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        bp.message = answer
            .and_then(|a| a.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(line) = answer.and_then(|a| a.get("line")).and_then(Value::as_u64)
            && bp.line.is_some()
        {
            bp.line = Some(line);
        }
        if let Some(reference) = answer
            .and_then(|a| a.get("instructionReference"))
            .and_then(Value::as_str)
            && bp.reference.is_some()
        {
            bp.reference = Some(reference.to_owned());
        }
    }
}

/// How a list is edited.
pub enum Edit {
    /// Add or replace one breakpoint.
    Set(Box<Breakpoint>),
    /// Remove the breakpoints that `matches` selects.
    Remove(Box<dyn Fn(&Breakpoint) -> bool + Send>),
}

fn sort_key(b: &Breakpoint) -> (u64, String, i64) {
    (
        b.line.unwrap_or(0),
        b.name
            .clone()
            .or_else(|| b.reference.clone())
            .or_else(|| b.data_id.clone())
            .unwrap_or_default(),
        b.offset.unwrap_or(0),
    )
}

impl Session {
    /// Edit one breakpoint list and bring every live target to it.
    ///
    /// # Errors
    ///
    /// No session, or the focused adapter rejects the list (nothing changes then).
    pub async fn edit_breakpoints(
        &self,
        kind: Kind,
        edit: Edit,
        timeout: Duration,
    ) -> Result<BreakpointResult> {
        // Held across the requests: concurrent edits would overwrite each other's lists.
        let mut desired = self.bps.lock().await;
        let mut next = desired.list(&kind);
        match edit {
            Edit::Set(new) => {
                next.retain(|b| !same_key(b, &new));
                next.push(*new);
                next.sort_by_key(sort_key);
            }
            Edit::Remove(matches) => next.retain(|b| !matches(b)),
        }
        let (origin, client) = self.active()?;
        self.ensure_configured(&origin, &client, timeout).await?;
        let (command, args) = request_for(&kind, &next);
        let response = client.request(command, args.clone(), timeout).await?;
        apply_response(&mut next, &response);
        desired.store(&kind, next.clone());

        let others: Vec<(String, Arc<DapClient>)> = self.shared.read(|s| {
            s.targets
                .iter()
                .filter(|t| t.id != origin && t.initialized && t.status != Status::Terminated)
                .filter_map(|t| t.client.clone().map(|c| (t.id.clone(), c)))
                .collect()
        });
        let mut warnings = Vec::new();
        for (id, other) in others {
            if let Err(e) = other.request(command, args.clone(), timeout).await {
                warnings.push(format!("{id}: {e}"));
            }
        }
        Ok(BreakpointResult {
            breakpoints: next,
            warnings,
        })
    }

    /// Give a target that just started every breakpoint of the session (before its
    /// `configurationDone`); a refusal is not fatal for the target.
    pub(crate) async fn apply_breakpoints(&self, id: &str, client: &DapClient) {
        let desired = self.bps.lock().await;
        let mut lists: Vec<(Kind, Vec<Breakpoint>)> = desired
            .source
            .iter()
            .map(|(p, l)| (Kind::Source(p.clone()), l.clone()))
            .collect();
        lists.push((Kind::Function, desired.function.clone()));
        lists.push((Kind::Instruction, desired.instruction.clone()));
        lists.push((Kind::Data, desired.data.clone()));
        drop(desired);
        for (kind, list) in lists.into_iter().filter(|(_, l)| !l.is_empty()) {
            let (command, args) = request_for(&kind, &list);
            if let Err(e) = client.request(command, args, self.timeout).await {
                self.shared.with(|s| {
                    s.output.push(&format!(
                        "[hrn] {id}: could not bind breakpoints ({command}): {e}\n"
                    ));
                });
            }
        }
    }
}

fn same_key(a: &Breakpoint, b: &Breakpoint) -> bool {
    if a.line.is_some() || b.line.is_some() {
        return a.line == b.line;
    }
    if a.name.is_some() || b.name.is_some() {
        return a.name == b.name;
    }
    if a.reference.is_some() || b.reference.is_some() {
        return a.reference == b.reference && a.offset == b.offset;
    }
    a.data_id == b.data_id
}
