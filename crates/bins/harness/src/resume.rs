//! `hrn -c`, `--resume`, `--restore`, and `[session] auto_resume`: startup session resolution.

use crate::cli::ChatArgs;
use crate::term::err_line;
use hrn_kernel::{HrnError, Result, SessionMeta, SessionStore};
use std::path::Path;

/// Which stored session (if any) `hrn chat` opens on startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartupSession {
    /// Start on the landing screen with no session loaded yet.
    Fresh,
    /// Open `id` and repair any interrupted tail without sending an LLM request.
    Resume(String),
    /// Open `id` and continue any interrupted turn (`--restore`).
    Restore(String),
}

/// A path as one comparable string: forward slashes, no `\\?\` prefix, case-folded on Windows.
fn normalized(path: &str) -> String {
    let path = path
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path
    }
}

/// The id of the most recently updated session that ran in `cwd` and has a message; `sessions`
/// is newest first. A session nobody wrote to is a stub, not something to continue.
pub(crate) fn latest_in_cwd(sessions: &[SessionMeta], cwd: &Path) -> Option<String> {
    let here = normalized(&cwd.to_string_lossy());
    sessions
        .iter()
        .filter(|s| s.head_node_id.is_some() && normalized(&s.cwd) == here)
        .max_by_key(|s| s.updated_at_ms)
        .map(|s| s.id.clone())
}

/// Up to `limit` most recently updated non-empty sessions for `cwd`, newest first.
pub(crate) fn recent_in_cwd(
    sessions: &[SessionMeta],
    cwd: &Path,
    limit: usize,
) -> Vec<SessionMeta> {
    let here = normalized(&cwd.to_string_lossy());
    let mut matching: Vec<SessionMeta> = sessions
        .iter()
        .filter(|s| s.head_node_id.is_some() && normalized(&s.cwd) == here)
        .cloned()
        .collect();
    matching.sort_by_key(|s| std::cmp::Reverse(s.updated_at_ms));
    matching.truncate(limit);
    matching
}

/// Resolve which session `hrn chat` opens based on `--restore`, `--continue`, `--resume`,
/// `--no-session`, and `[session] auto_resume`.
///
/// # Errors
/// Fails when the session store cannot be read or an explicit session prefix does not match.
pub(crate) fn resolve_startup_session(
    store: &dyn SessionStore,
    cwd: &Path,
    args: &ChatArgs,
    auto_resume: bool,
) -> Result<StartupSession> {
    if args.no_session {
        return Ok(StartupSession::Fresh);
    }
    if let Some(raw) = args.restore.as_deref() {
        let wanted = raw.trim();
        let target = if wanted.is_empty() {
            continue_target(store, cwd)?
        } else {
            Some(find_session(store, wanted)?)
        };
        return Ok(target.map_or(StartupSession::Fresh, StartupSession::Restore));
    }
    if args.continue_session {
        return Ok(
            continue_target(store, cwd)?.map_or(StartupSession::Fresh, StartupSession::Resume)
        );
    }
    if let Some(wanted) = args.resume.as_deref() {
        return Ok(StartupSession::Resume(find_session(store, wanted)?));
    }
    if auto_resume && args.goal.is_none() {
        let found = latest_in_cwd(&store.list_sessions(usize::MAX)?, cwd);
        return Ok(found.map_or(StartupSession::Fresh, StartupSession::Resume));
    }
    Ok(StartupSession::Fresh)
}

/// What `--continue` resumes in `cwd`: `None` (with a note) when there is nothing to continue.
///
/// # Errors
/// The session list could not be read.
pub(crate) fn continue_target(store: &dyn SessionStore, cwd: &Path) -> Result<Option<String>> {
    let found = latest_in_cwd(&store.list_sessions(usize::MAX)?, cwd);
    if found.is_none() {
        err_line("no earlier session in this directory · starting a new one");
    }
    Ok(found)
}

/// An exact session id, or the one session whose id starts with `wanted`.
pub(crate) fn find_session(store: &dyn SessionStore, wanted: &str) -> Result<String> {
    if store.get_session(wanted)?.is_some() {
        return Ok(wanted.to_owned());
    }
    let matches: Vec<String> = store
        .list_sessions(usize::MAX)?
        .into_iter()
        .filter(|s| s.id.starts_with(wanted))
        .map(|s| s.id)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(HrnError::Config(format!("no session matches `{wanted}`"))),
        _ => Err(HrnError::Config(format!(
            "`{wanted}` matches {} sessions; give more of the id",
            matches.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrn_kernel::{AgentStatus, PermissionMode, ThinkingLevel};

    fn meta(id: &str, cwd: &str, head: Option<&str>) -> SessionMeta {
        SessionMeta {
            pinned: false,
            id: id.into(),
            title: "t".into(),
            cwd: cwd.into(),
            branch: None,
            head_node_id: head.map(Into::into),
            model: None,
            thinking: ThinkingLevel::Medium,
            goal: None,
            permission_mode: PermissionMode::Default,
            parent_session_id: None,
            parent_node_id: None,
            status: AgentStatus::Idle,
            title_source: None,
            exit_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn picks_the_newest_written_session_of_this_directory() {
        let mut newest = meta("newest", "/work/a/", Some("n"));
        newest.updated_at_ms = 200;
        let mut older = meta("older", "/work/a", Some("n"));
        older.updated_at_ms = 100;
        let sessions = [
            meta("stub", "/work/a", None),
            meta("other", "/work/b", Some("n")),
            newest,
            older,
        ];
        assert_eq!(
            latest_in_cwd(&sessions, Path::new("/work/a")).as_deref(),
            Some("newest")
        );
        assert_eq!(latest_in_cwd(&sessions, Path::new("/work/c")), None);
    }

    #[test]
    fn auto_resume_lands_in_newest_cwd_session() {
        let store = hrn_edge_store::SqliteStore::open_in_memory().expect("store");
        let mut old_here = meta("s_old", "/proj/app", Some("n1"));
        old_here.updated_at_ms = 100;
        old_here.model = Some("smol".into());
        old_here.thinking = ThinkingLevel::Low;
        old_here.permission_mode = PermissionMode::Default;

        let mut new_here = meta("s_new", "/proj/app", Some("n2"));
        new_here.updated_at_ms = 500;
        new_here.model = Some("slow".into());
        new_here.thinking = ThinkingLevel::High;
        new_here.permission_mode = PermissionMode::AutoEdit;

        let mut newer_elsewhere = meta("s_other", "/proj/other", Some("n3"));
        newer_elsewhere.updated_at_ms = 900;

        for s in [&old_here, &new_here, &newer_elsewhere] {
            store.create_session(s).expect("create");
        }

        let bare = ChatArgs::default();
        assert_eq!(
            resolve_startup_session(&store, Path::new("/proj/app"), &bare, false).expect("off"),
            StartupSession::Fresh
        );
        let picked =
            resolve_startup_session(&store, Path::new("/proj/app"), &bare, true).expect("on");
        assert_eq!(picked, StartupSession::Resume("s_new".into()));
        let loaded = store
            .get_session("s_new")
            .expect("get")
            .expect("session exists");
        assert_eq!(loaded.model.as_deref(), Some("slow"));
        assert_eq!(loaded.thinking, ThinkingLevel::High);
        assert_eq!(loaded.permission_mode, PermissionMode::AutoEdit);

        let no_sess = ChatArgs {
            no_session: true,
            ..ChatArgs::default()
        };
        assert_eq!(
            resolve_startup_session(&store, Path::new("/proj/app"), &no_sess, true)
                .expect("no-session"),
            StartupSession::Fresh
        );
    }

    #[test]
    fn windows_spellings_of_one_directory_match() {
        let sessions = [meta("s", r"\\?\C:\Work\A", Some("n"))];
        let found = latest_in_cwd(&sessions, Path::new("C:/work/a"));
        assert_eq!(found.is_some(), cfg!(windows));
    }
}
