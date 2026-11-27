//! The tool commands: a command of the agent's shell (`read notes.txt`, `grep --pattern x`) runs
//! a tool of the *same session*. The shell side is a function generated into the bash rc file
//! ([`write_rc`]) which calls `hrn tool <name> …`; the shim dials a loopback socket here, and the
//! call is executed through [`hrn_kernel::ToolCaller`] — the same path as a call the model made,
//! so permission mode, approval prompt and hooks gate it identically.
//!
//! One listener and one token per process. Each live `bash` call publishes its context under a
//! numeric id and exports it in `HRN_TOOL_SOCK`, so a command addresses the call that started it;
//! a command of a finished call, or of a shell where the session offers no tool calling, runs the
//! real `read`/`grep` instead.

use crate::tool_cli::{self, Invocation};
use hrn_edge_ipc::{BridgeServer, Reply, ToolBridge};
use hrn_kernel::serde_json::Value;
use hrn_kernel::{BoxFuture, CancelFlag, HrnError, Result, ToolContext, ToolOutput};
use std::collections::BTreeMap;
use std::fmt::Write as _;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Environment of a tool command: the loopback endpoint `<port>:<token>:<call id>`, empty when
/// the running shell line has none.
pub(crate) const SOCK_VAR: &str = "HRN_TOOL_SOCK";
/// Environment naming this `hrn` executable (the shim the rc functions call).
pub(crate) const EXE_VAR: &str = "HRN_TOOL_EXE";
/// Environment keeping the user's own `BASH_ENV` when the generated rc file takes its place.
const USER_BASH_ENV_VAR: &str = "HRN_USER_BASH_ENV";

/// Shell names that must keep working as shell words: a command line like `while read line` is
/// not a tool command, and only the argument shapes below route to one.
const SHELL_NAMES: &str = include_str!("../data/bash-shell-names.txt");

/// The process-wide bridge.
#[derive(Default)]
struct Hub {
    live: Mutex<BTreeMap<u64, ToolContext>>,
    next: AtomicU64,
    server: Mutex<Option<(BridgeServer, String)>>,
    rc_names: Mutex<Option<Vec<String>>>,
}

static HUB: OnceLock<Hub> = OnceLock::new();
/// Path of the generated rc file, set when the module registers.
static RC_PATH: OnceLock<PathBuf> = OnceLock::new();

fn hub() -> &'static Hub {
    HUB.get_or_init(Hub::default)
}

impl Hub {
    /// Bind the listener once; it answers with the token it guards.
    fn endpoint(&self) -> Result<(SocketAddr, String)> {
        let mut slot = self
            .server
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((server, token)) = slot.as_ref() {
            return Ok((server.addr(), token.clone()));
        }
        let token = hrn_edge_ipc::generate_token()?;
        let server = BridgeServer::start(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            token.clone(),
            Arc::new(Serving),
        )?;
        let made = (server, token.clone());
        *slot = Some(made);
        match slot.as_ref() {
            Some((server, stored)) => Ok((server.addr(), stored.clone())),
            None => Err(HrnError::Internal("tool bridge vanished".to_owned())),
        }
    }

    fn publish(&self, ctx: &ToolContext) -> Result<Publication> {
        let (address, token) = self.endpoint()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.lock_live().insert(id, ctx.clone());
        // The listener is loopback, so the host is implied; the token is hex and the id decimal,
        // so `:` separates the three parts unambiguously.
        Ok(Publication {
            id,
            env: BTreeMap::from([(
                SOCK_VAR.to_owned(),
                format!("{}:{token}:{id}", address.port()),
            )]),
        })
    }

    fn finish(&self, id: u64) {
        let _ = self.lock_live().remove(&id);
    }

    fn context(&self, id: u64) -> Option<ToolContext> {
        self.lock_live().get(&id).cloned()
    }

    fn lock_live(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, ToolContext>> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A live `bash` call's bridge registration; dropping it unhooks the call.
pub(crate) struct Publication {
    id: u64,
    env: BTreeMap<String, String>,
}

impl Publication {
    /// Hook `ctx` into the bridge for as long as the value lives, and make sure the shell rc file
    /// defines a command for every tool of the session. `None` when the session gives tools no
    /// way to call each other (a unit test, a detached tool).
    ///
    /// # Errors
    ///
    /// When the bridge or the rc file cannot be set up.
    pub(crate) fn new(ctx: &ToolContext) -> Result<Option<Self>> {
        let Some(caller) = ctx.caller.as_ref() else {
            return Ok(None);
        };
        let names = caller.tool_names();
        ensure_rc(&names)?;
        Ok(Some(hub().publish(ctx)?))
    }

    /// The variables to export into the call's shell.
    #[must_use]
    pub(crate) fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        hub().finish(self.id);
    }
}

/// Write the rc file whenever the tool list changed (startup does zero disk work until the first
/// `bash` call, and a rebuild that kept the list rewrites nothing).
fn ensure_rc(names: &[String]) -> Result {
    let path = RC_PATH
        .get()
        .ok_or_else(|| HrnError::Internal("the shell module registered no rc path".to_owned()))?;
    let mut last = hub()
        .rc_names
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() == Some(names) {
        return Ok(());
    }
    write_rc(path, names)?;
    *last = Some(names.to_vec());
    Ok(())
}

/// What every shell of this process gets: the generated rc file (keeping the user's own
/// `BASH_ENV` reachable through `HRN_USER_BASH_ENV`), the executable the functions call, and an
/// empty endpoint until a call sets one.
pub(crate) fn shell_env() -> BTreeMap<String, String> {
    let mut vars = BTreeMap::from([
        (SOCK_VAR.to_owned(), String::new()),
        (EXE_VAR.to_owned(), current_exe()),
    ]);
    if let Some(path) = RC_PATH.get() {
        let ours = path.to_string_lossy().replace('\\', "/");
        if let Some(user) = std::env::var("BASH_ENV")
            .ok()
            .filter(|v| !v.is_empty() && *v != ours)
        {
            vars.insert(USER_BASH_ENV_VAR.to_owned(), user);
        }
        vars.insert("BASH_ENV".to_owned(), ours);
    }
    vars
}

/// Where the generated rc file lives.
#[must_use]
pub(crate) fn rc_path(data_dir: &Path) -> PathBuf {
    data_dir.join("bash-tools.bash")
}

/// Remember where [`write_rc`] will put the file.
pub(crate) fn set_rc_path(path: PathBuf) {
    let _ = RC_PATH.set(path);
}

/// One tool's shell function. `strict` names are shell words first (`while read line` stays the
/// builtin) and tool commands only in the argument shapes below.
fn function(name: &str, strict: bool) -> String {
    let tool = format!("__hrn_tool {name} \"$@\"");
    let plain = if strict {
        format!("builtin {name} \"$@\"")
    } else {
        format!("command {name} \"$@\"")
    };
    let mut out = format!("{name}() {{\n  case \"${{1:-}}\" in\n");
    if strict {
        let _ = write!(
            out,
            "    --) {plain};;\n    \
             --help|-h) if [ \"$#\" -eq 1 ] && [ -n \"${SOCK_VAR}\" ]; then {tool}; \
             else {plain}; fi;;\n    \
             -j|--json|--*|@*|*://*) if [ -n \"${SOCK_VAR}\" ]; then {tool}; \
             else {plain}; fi;;\n"
        );
    } else {
        let _ = write!(
            out,
            "    -j|--json) if [ -n \"${SOCK_VAR}\" ]; then {tool}; else {plain}; fi;;\n    \
             --help|-h) if [ \"$#\" -eq 1 ] && [ -n \"${SOCK_VAR}\" ]; then {tool}; \
             else {plain}; fi;;\n"
        );
    }
    let fall_back = if strict {
        tool
    } else {
        // An external command of that name is what the user means.
        format!(
            "if type -P {name} >/dev/null 2>&1; then {plain}; \
             elif [ -n \"${SOCK_VAR}\" ]; then {tool}; else {plain}; fi"
        )
    };
    let _ = write!(out, "    *) {fall_back};;\n  esac\n}}\n");
    out
}

/// The whole rc file for these tool names.
fn rc_text(names: &[String]) -> String {
    let mut body = String::from(
        "# Generated by hrn: every registered tool is callable as a command.\n\
         __hrn_tool() { \"$HRN_TOOL_EXE\" tool \"$@\"; }\n\
         if [ -n \"${HRN_USER_BASH_ENV:-}\" ]; then . \"$HRN_USER_BASH_ENV\"; fi\n",
    );
    for name in names {
        if name == "bash" || !valid_ident(name) {
            continue;
        }
        body.push_str(&function(name, is_shell_name(name)));
    }
    body
}

/// Names the shell owns.
fn is_shell_name(name: &str) -> bool {
    SHELL_NAMES.lines().any(|line| line == name)
}

/// Write the tool-command functions unless the file already holds exactly this list (startup
/// stays free of disk churn, and a running shell keeps a file it has already sourced).
///
/// # Errors
///
/// When the file cannot be written.
pub(crate) fn write_rc(path: &Path, names: &[String]) -> Result {
    // A temp name unique per process and call: concurrent writers (parallel backends in one test
    // binary, several `hrn` processes) must never rename each other's temp file away.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let body = rc_text(names);
    if std::fs::read_to_string(path).is_ok_and(|old| old == body) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| HrnError::Io(format!("create `{}`: {e}", dir.display())))?;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("bash.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&tmp, &body)
        .map_err(|e| HrnError::Io(format!("write `{}`: {e}", tmp.display())))?;
    let Err(err) = std::fs::rename(&tmp, path) else {
        return Ok(());
    };
    let _ = std::fs::remove_file(&tmp);
    // On Windows a replace can lose to a concurrent writer's replace, or to a handle open on the
    // file without delete sharing, with `ERROR_ACCESS_DENIED`. A file that already holds this
    // body is the result the caller wanted.
    if std::fs::read_to_string(path).is_ok_and(|old| old == body) {
        return Ok(());
    }
    Err(HrnError::Io(format!("install `{}`: {err}", path.display())))
}

/// A name the generated shell function can use.
fn valid_ident(name: &str) -> bool {
    !name.is_empty()
        && name != "-"
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn current_exe() -> String {
    std::env::current_exe().map_or_else(
        |_| "hrn".to_owned(),
        |p| p.to_string_lossy().replace('\\', "/"),
    )
}

/// The bridge handler: parse the command line against the tool's schema and run it as the call.
struct Serving;

impl ToolBridge for Serving {
    fn handle(&self, request: String, cancel: CancelFlag) -> BoxFuture<Result<Reply>> {
        Box::pin(async move {
            let line = hrn_kernel::serde_json::from_str::<Value>(&request)
                .map_err(|e| HrnError::Tool(format!("bad tool-command request: {e}")))?;
            Ok(answer(&line, cancel).await)
        })
    }
}

async fn answer(line: &Value, cancel: CancelFlag) -> Reply {
    let id = line.get("cid").and_then(Value::as_u64).unwrap_or_default();
    let Some(ctx) = hub().context(id) else {
        return Reply::err(
            1,
            format!("hrn tool: no `bash` call {id} is running; its shell line has ended\n"),
        );
    };
    let Some(caller) = ctx.caller.clone() else {
        return Reply::err(
            1,
            "hrn tool: this session gives tools no way to call tools\n".to_owned(),
        );
    };
    let name = line.get("tool").and_then(Value::as_str).unwrap_or("");
    let argv: Vec<String> = line
        .get("argv")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let files: BTreeMap<usize, String> = line
        .get("files")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    if name == "bash" {
        return Reply::err(
            2,
            "hrn tool: `bash` is the shell this command runs in; type the command itself\n"
                .to_owned(),
        );
    }
    let Some(spec) = caller.spec(name) else {
        return Reply::err(
            2,
            format!(
                "hrn tool: `{name}` is not a tool of this session. Tools: {}\n",
                caller.tool_names().join(", ")
            ),
        );
    };
    let fields = match tool_cli::parse(&spec.parameters, &argv, &files) {
        Ok(Invocation::Help) => {
            return Reply {
                code: 0,
                stdout: tool_cli::help(name, &spec.description, &spec.parameters).into_bytes(),
                stderr: Vec::new(),
            };
        }
        Ok(Invocation::Call(fields)) => fields,
        Err(err) => return Reply::err(2, format!("hrn tool {name}: {err}\n")),
    };
    let output = caller
        .call(name, fields, ctx.cwd.clone(), cancel)
        .await
        .unwrap_or_else(|err| ToolOutput::err(err.to_string()));
    render(&output)
}

/// A tool output as the shell sees it: text on stdout (stderr when it failed), images saved to a
/// scratch file whose path is named in the text.
fn render(output: &ToolOutput) -> Reply {
    let mut text = output.content.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for image in &output.images {
        let _ = writeln!(text, "image: {}", save_image(image));
    }
    let body = text.into_bytes();
    if output.is_error {
        Reply {
            code: 1,
            stdout: Vec::new(),
            stderr: body,
        }
    } else {
        Reply {
            code: 0,
            stdout: body,
            stderr: Vec::new(),
        }
    }
}

/// Write an inline image to a scratch file and name it; `read` takes that path.
fn save_image(image: &hrn_kernel::ImageAttachment) -> String {
    let dir = std::env::temp_dir().join("hrn-images");
    let made = || -> Result<PathBuf> {
        std::fs::create_dir_all(&dir)?;
        let stem = image.label.as_deref().map_or("image".to_owned(), |s| {
            Path::new(s).file_stem().map_or_else(
                || "image".to_owned(),
                |stem| stem.to_string_lossy().into_owned(),
            )
        });
        let ext = image
            .media_type
            .rsplit('/')
            .next()
            .filter(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric()))
            .unwrap_or("png");
        let path = dir.join(format!("{stem}-{}.{ext}", nanos()));
        std::fs::write(
            &path,
            hrn_edge_tools::img::decode_base64(&image.data_base64)?,
        )?;
        Ok(path)
    };
    match made() {
        Ok(path) => path.to_string_lossy().replace('\\', "/"),
        Err(err) => format!("<image not saved: {err}>"),
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_word_stays_the_shell_and_tool_shapes_route() {
        let rc = rc_text(&[
            "read".to_owned(),
            "web_search".to_owned(),
            "bash".to_owned(),
        ]);
        // `read` is a shell builtin: only tool-shaped arguments may reach the tool.
        let strict = rc.split("read() {").nth(1).expect("read function");
        assert!(strict.contains("builtin read \"$@\""), "{strict}");
        assert!(strict.contains("--help|-h)"), "{strict}");
        assert!(strict.contains("-j|--json|--*|@*|*://*)"), "{strict}");
        assert!(!strict.contains("builtin bash"), "{strict}");
        assert!(
            !rc.contains("bash() {"),
            "the shell cannot call itself: {rc}"
        );
        // A tool with no shell meaning is the tool unless an external command of that name exists.
        let loose = rc
            .split("web_search() {")
            .nth(1)
            .expect("web_search function");
        assert!(loose.contains("type -P web_search"), "{loose}");
        assert!(loose.contains("command web_search \"$@\""), "{loose}");
    }

    #[test]
    fn only_tool_names_become_functions() {
        let rc = rc_text(&["bad-name".to_owned(), "9lives".to_owned(), "lsp".to_owned()]);
        assert!(!rc.contains("bad-name()"), "{rc}");
        assert!(!rc.contains("9lives()"), "{rc}");
        assert!(rc.contains("lsp() {"), "{rc}");
    }

    #[test]
    fn a_base64_attachment_lands_on_disk() {
        let image = hrn_kernel::ImageAttachment {
            media_type: "image/png".to_owned(),
            data_base64: "aGk=".to_owned(),
            label: Some("probe.png".to_owned()),
            width: None,
            height: None,
        };
        let path = save_image(&image);
        let text = std::fs::read_to_string(&path).expect("saved image");
        let _ = std::fs::remove_file(&path);
        assert_eq!(text, "hi");
    }
}
