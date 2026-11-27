//! Module registration: a module declares what it contributes on a [`Registry`], and the binary
//! finishes the registry once into a [`Registered`] set the runtime is assembled from.
//!
//! The registry is plain data: no I/O, no executor. Everything a module needs from the process
//! (working directory, data directory, its `[<id>]` config table, values shared across rebuilds)
//! arrives through it, so a module crate depends on the kernel and edge crates only.

use crate::error::{CancelFlag, HrnError, Result};
use crate::help::HelpPages;
use crate::ports::EvalHost;
use crate::proc::ProcSource;
use crate::traits::{BoxFuture, DeferredTool, HarnessHook, LlmProvider, Tool};
use crate::urls::UrlScheme;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::any::{Any, TypeId};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Facts about the running process, the same for every module.
#[derive(Clone, Debug)]
pub struct Host {
    /// Working directory of the session being built.
    pub cwd: PathBuf,
    /// Per-user data directory (databases, scratch, runner scripts).
    pub data_dir: PathBuf,
}

/// Values that outlive one runtime build (a `/login` hot-reload rebuilds the runtime but keeps
/// running jobs, open clients and the like), keyed by their type.
#[derive(Default)]
pub struct Shared {
    slots: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Shared {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// The value of type `T`, created with `init` the first time anyone asks.
    ///
    /// # Errors
    ///
    /// Whatever `init` returns.
    pub fn get_or_init<T: Any + Send + Sync>(
        &self,
        init: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(found) = slots.get(&TypeId::of::<T>()) {
            return Arc::clone(found).downcast::<T>().map_err(|_| {
                HrnError::Internal("a shared slot holds a value of another type".to_owned())
            });
        }
        let made = Arc::new(init()?);
        slots.insert(TypeId::of::<T>(), made.clone());
        Ok(made)
    }
}

/// What a module command wants shown and how the process should exit. The binary does the
/// printing: a module never touches the terminal.
#[derive(Clone, Debug, Default)]
pub struct CommandOutcome {
    /// Text for stdout.
    pub stdout: String,
    /// Text for stderr.
    pub stderr: String,
    /// Process exit code.
    pub code: u8,
}

/// The body of a module command: the arguments after the command name, plus a flag the binary
/// raises on Ctrl-C.
pub type CommandFn =
    Arc<dyn Fn(Vec<String>, CancelFlag) -> BoxFuture<Result<CommandOutcome>> + Send + Sync>;

/// A top-level `hrn <name> ...` command a module contributes.
#[derive(Clone)]
pub struct Command {
    /// The command word.
    pub name: &'static str,
    /// One line for the command listing.
    pub about: &'static str,
    /// What it does.
    pub run: CommandFn,
}

/// Everything the modules registered, in registration order.
#[derive(Default)]
pub struct Registered {
    /// Tools whose schemas are sent with every request.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Tools the registering module marked deferrable: `(group, name, tool)` triples the
    /// binary splits into live or deferred by policy.
    pub deferrable: Vec<(String, String, Arc<dyn Tool>)>,
    /// Tools found through `tool_search`.
    pub deferred: Vec<DeferredTool>,
    /// Internal-URL schemes.
    pub schemes: Vec<Arc<dyn UrlScheme>>,
    /// Lifecycle hooks, run in this order.
    pub hooks: Vec<Arc<dyn HarnessHook>>,
    /// Background work addressable under `proc://`.
    pub procs: Vec<Arc<dyn ProcSource>>,
    /// Paragraphs appended to the system prompt.
    pub prompt_sections: Vec<String>,
    /// Reference pages served under `help://`, keyed by page name.
    pub help_pages: HelpPages,
    /// Top-level `hrn <name>` commands.
    pub commands: Vec<Command>,
}

/// What a module may declare while registering. The binary threads one registry through every
/// module in build order and finishes it once at the end.
pub struct Registry {
    host: Host,
    config: BTreeMap<String, Value>,
    llm: Option<Arc<dyn LlmProvider>>,
    eval_host: Option<Arc<dyn EvalHost>>,
    shared: Arc<Shared>,
    registering: Option<&'static str>,
    /// Which module owns each tool / scheme name, so a clash names both.
    owners: HashMap<String, &'static str>,
    out: Registered,
}

/// Whether `id` is one lowercase name segment.
fn is_valid_id(id: &str) -> bool {
    id.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Registry {
    /// A registry over `host`, with the module config tables of the user's file and the
    /// process-wide `shared` store.
    pub fn new(host: Host, config: BTreeMap<String, Value>, shared: Arc<Shared>) -> Self {
        Self {
            host,
            config,
            llm: None,
            eval_host: None,
            shared,
            registering: None,
            owners: HashMap::new(),
            out: Registered::default(),
        }
    }

    /// Hand modules the process's model router, for the ones that call a model themselves (the
    /// `find` tool scores excerpts with a fast role).
    #[must_use]
    pub fn with_llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// The model router, absent for a registry built without one (module commands).
    pub fn llm(&self) -> Option<Arc<dyn LlmProvider>> {
        self.llm.clone()
    }

    /// Hand `modules/eval` the host bridge for tool calls, subagent spawns, and completions.
    #[must_use]
    pub fn with_eval_host(mut self, host: Arc<dyn EvalHost>) -> Self {
        self.eval_host = Some(host);
        self
    }

    /// The eval host bridge, absent outside a live agent runtime.
    pub fn eval_host(&self) -> Option<Arc<dyn EvalHost>> {
        self.eval_host.clone()
    }

    /// Run one module's registration under the module id `id`: one lowercase name segment, given
    /// explicitly. `kernel`, `hrn` and `xtask` are reserved for the kernel, the binary and the
    /// tooling. A failing step fails startup.
    ///
    /// # Errors
    ///
    /// A malformed or reserved `id`; otherwise whatever the module returns.
    pub fn module(
        mut self,
        id: &'static str,
        register: impl FnOnce(&mut Self) -> Result,
    ) -> Result<Self> {
        if !is_valid_id(id) || matches!(id, "kernel" | "hrn" | "xtask") {
            return Err(HrnError::Config(format!("`{id}` is not a valid module id")));
        }
        self.registering = Some(id);
        let outcome = register(&mut self);
        self.registering = None;
        outcome.map_err(|e| match e {
            HrnError::Config(msg) => HrnError::Config(format!("module `{id}`: {msg}")),
            other => other,
        })?;
        Ok(self)
    }

    /// The process facts.
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The process-wide shared store.
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// The shared store behind an `Arc`, for a component that outlives registration and must
    /// hold onto it (the page fetcher builds its HTTP client on first use).
    pub fn shared_arc(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// The module's own `[<id>]` table of the config file parsed as `T` (`T::default()` when the
    /// file has none). `T` should reject unknown keys: this is where a typo in the section is
    /// caught.
    ///
    /// # Errors
    ///
    /// `HrnError::Config` when the table does not parse, or when called outside a module step.
    pub fn config<T: DeserializeOwned + Default>(&mut self) -> Result<T> {
        let id = self.current()?;
        match self.config.remove(id) {
            None => Ok(T::default()),
            Some(table) => {
                serde_json::from_value(table).map_err(|e| HrnError::Config(format!("[{id}]: {e}")))
            }
        }
    }

    /// Register a tool sent with every request.
    ///
    /// # Errors
    ///
    /// When another module already registered a tool of the same name.
    pub fn tool(&mut self, tool: Arc<dyn Tool>) -> Result {
        let name = tool.spec().name;
        self.claim("tool", &name)?;
        self.out.tools.push(tool);
        Ok(())
    }

    /// Register a tool that is loaded only when the binary's policy keeps premium tools live;
    /// by default it joins the `tool_search` catalog under this module's group instead.
    ///
    /// # Errors
    ///
    /// When another module already registered a tool of the same name.
    pub fn deferrable_tool(&mut self, tool: Arc<dyn Tool>) -> Result {
        let group = self.current()?.to_owned();
        let name = tool.spec().name;
        self.claim("tool", &name)?;
        self.out.deferrable.push((group, name, tool));
        Ok(())
    }

    /// Register tools found through `tool_search`.
    ///
    /// # Errors
    ///
    /// When a tool name is already taken.
    pub fn deferred(&mut self, tools: impl IntoIterator<Item = DeferredTool>) -> Result {
        for tool in tools {
            self.claim("tool", &tool.tool.spec().name)?;
            self.out.deferred.push(tool);
        }
        Ok(())
    }

    /// Register an internal-URL scheme.
    ///
    /// # Errors
    ///
    /// When another module already registered the scheme.
    pub fn scheme(&mut self, scheme: Arc<dyn UrlScheme>) -> Result {
        self.claim("scheme", scheme.scheme())?;
        self.out.schemes.push(scheme);
        Ok(())
    }

    /// Register a lifecycle hook; hooks run in registration order.
    pub fn hook(&mut self, hook: Arc<dyn HarnessHook>) {
        self.out.hooks.push(hook);
    }

    /// Register a kind of background work shown under `proc://`.
    pub fn proc_source(&mut self, source: Arc<dyn ProcSource>) {
        self.out.procs.push(source);
    }

    /// Register a top-level `hrn <name>` command.
    ///
    /// # Errors
    ///
    /// When another module already registered the command word.
    pub fn command(&mut self, command: Command) -> Result {
        self.claim("command", command.name)?;
        self.out.commands.push(command);
        Ok(())
    }

    /// Append a paragraph to the system prompt.
    pub fn prompt_section(&mut self, text: impl Into<String>) {
        self.out.prompt_sections.push(text.into());
    }

    /// Contribute one `help://<page>` reference page (the text a tool description points to
    /// instead of inlining). Re-registering a page name replaces it.
    pub fn help_page(&mut self, page: impl Into<String>, body: impl Into<String>) {
        self.out.help_pages.insert(page.into(), body.into());
    }

    /// Freeze the registrations.
    ///
    /// # Errors
    ///
    /// `HrnError::Config` naming every config section that no installed module read.
    pub fn finish(self) -> Result<Registered> {
        if self.config.is_empty() {
            return Ok(self.out);
        }
        let names: Vec<String> = self.config.keys().map(|k| format!("[{k}]")).collect();
        Err(HrnError::Config(format!(
            "unknown config section(s) {}: not core settings and no installed module reads them",
            names.join(", ")
        )))
    }

    fn current(&self) -> Result<&'static str> {
        self.registering.ok_or_else(|| {
            HrnError::Internal("a module-only registry call ran outside a module step".to_owned())
        })
    }

    fn claim(&mut self, kind: &str, name: &str) -> Result {
        let id = self.current()?;
        let key = format!("{kind}:{name}");
        if let Some(first) = self.owners.insert(key, id) {
            return Err(HrnError::Config(format!(
                "{kind} `{name}` is registered by both `{first}` and `{id}`"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoxFuture, ToolContext, ToolOutput, ToolSpec};
    use serde::Deserialize;
    use serde_json::json;

    struct Named(&'static str);

    impl Tool for Named {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.to_owned(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }

        fn execute(&self, _: &ToolContext, _: Value) -> BoxFuture<Result<ToolOutput>> {
            Box::pin(async { Ok(ToolOutput::ok("")) })
        }
    }

    #[derive(Debug, Default, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Cfg {
        #[serde(default)]
        depth: u8,
    }

    fn registry(config: Value) -> Registry {
        let table = match config {
            Value::Object(map) => map.into_iter().collect(),
            _ => BTreeMap::new(),
        };
        let host = Host {
            cwd: PathBuf::from("."),
            data_dir: PathBuf::from("."),
        };
        Registry::new(host, table, Arc::new(Shared::new()))
    }

    #[test]
    fn a_module_reads_only_its_own_typed_table() {
        let reg = registry(json!({"a": {"depth": 3}, "b": {"depth": 9}}));
        let reg = reg
            .module("a", |r| {
                assert_eq!(r.config::<Cfg>()?, Cfg { depth: 3 });
                Ok(())
            })
            .expect("a registers");
        // `[b]` has no installed module: finishing names it.
        let err = reg.finish().err().expect("unclaimed section").to_string();
        assert!(err.contains("[b]"), "{err}");
    }

    #[test]
    fn an_unknown_key_in_the_module_table_is_refused() {
        let err = registry(json!({"a": {"depht": 3}}))
            .module("a", |r| r.config::<Cfg>().map(|_| ()))
            .err()
            .expect("typo refused")
            .to_string();
        assert!(err.contains("[a]") && err.contains("depht"), "{err}");
    }

    #[test]
    fn two_modules_cannot_register_the_same_tool() {
        let err = registry(json!({}))
            .module("a", |r| r.tool(Arc::new(Named("x"))))
            .and_then(|r| r.module("b", |r| r.tool(Arc::new(Named("x")))))
            .err()
            .expect("clash")
            .to_string();
        assert!(err.contains("`a`") && err.contains("`b`"), "{err}");
    }

    #[test]
    fn shared_values_are_created_once() {
        let shared = Shared::new();
        let one = shared.get_or_init(|| Ok(7_u32)).expect("init");
        let two = shared
            .get_or_init(|| Err::<u32, _>(HrnError::Internal("not called".into())))
            .expect("cached");
        assert!(Arc::ptr_eq(&one, &two));
    }
}
