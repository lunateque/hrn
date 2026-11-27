//! `hrn sessions` and `hrn config check`.

use crate::auth::provider_key_report;
use crate::config::Config;
use crate::term::{out, short};
use hrn_edge_llm::{ProviderKind, resolve_alias};
use hrn_harness::{discover_custom_commands, discover_skills, load_subagent_profiles};
use hrn_kernel::{HrnError, Result, SessionMeta, SessionStore, WorkspaceFs};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

/// Format Unix milliseconds as `YYYY-MM-DD HH:MM:SS` (UTC).
pub(crate) fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (H. Hinnant), valid for the whole proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn session_row(s: &SessionMeta, pinned: bool) -> String {
    format!(
        "{}\t{}\t{}\t{:?}\t{}\t{}\t{}\n",
        s.id,
        if pinned { "pinned" } else { "-" },
        format_utc(s.updated_at_ms),
        s.status,
        s.model.as_deref().unwrap_or("-"),
        s.cwd,
        s.title
    )
}

/// Print every stored top-level session (tab-separated, pinned first, then newest first).
pub(crate) fn sessions(store: &dyn SessionStore) -> Result {
    let list = store.list_sessions(usize::MAX)?;
    if list.is_empty() {
        out("no sessions\n");
        return Ok(());
    }
    let pinned: BTreeSet<String> = store.pinned_sessions()?.into_iter().collect();
    out("ID\tPIN\tUPDATED (UTC)\tSTATUS\tMODEL\tCWD\tTITLE\n");
    for s in &list {
        out(&session_row(s, pinned.contains(&s.id)));
    }
    Ok(())
}

/// Ask `question` on the terminal; anything but `y`/`yes` (or a closed stdin) is a no.
pub(crate) fn ask(question: &str) -> bool {
    out(&format!("{question} [y/N] "));
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0 {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// An existing session by id or unique id prefix.
fn resolve(store: &dyn SessionStore, wanted: &str) -> Result<SessionMeta> {
    crate::resume::find_session(store, wanted)
        .and_then(|id| store.get_session(&id))?
        .ok_or_else(|| HrnError::Store(format!("unknown session `{wanted}`")))
}

/// Delete a session, its messages, its usage and its subagent sessions, asking `confirm` first
/// unless `force`.
///
/// # Errors
/// The store could not be read, or `wanted` names no single session.
pub(crate) fn delete_session(
    store: &dyn SessionStore,
    wanted: &str,
    force: bool,
    confirm: &dyn Fn(&str) -> bool,
) -> Result {
    let session = resolve(store, wanted)?;
    let nodes = store.load_tree(&session.id)?.len();
    if !force
        && !confirm(&format!(
            "delete session {} ({}, {nodes} node(s))? This cannot be undone.",
            session.id,
            short(&session.title, 60)
        ))
    {
        out("kept\n");
        return Ok(());
    }
    if store.delete_session(&session.id)? {
        out(&format!(
            "deleted session {} ({nodes} node(s))\n",
            session.id
        ));
    } else {
        out("it was already gone\n");
    }
    Ok(())
}

/// Set a session's title.
pub(crate) fn rename_session(store: &dyn SessionStore, wanted: &str, title: &str) -> Result {
    let session = resolve(store, wanted)?;
    let title = title.trim();
    if title.is_empty() {
        return Err(HrnError::Config(
            "a session title cannot be empty".to_owned(),
        ));
    }
    store.rename_session(&session.id, title, hrn_kernel::TitleSource::User)?;
    out(&format!("session {} is now titled `{title}`\n", session.id));
    Ok(())
}

/// Pin or unpin a session.
pub(crate) fn pin_session(store: &dyn SessionStore, wanted: &str, pinned: bool) -> Result {
    let session = resolve(store, wanted)?;
    store.set_pinned(&session.id, pinned)?;
    out(&format!(
        "session {} {}\n",
        session.id,
        if pinned { "pinned" } else { "unpinned" }
    ));
    Ok(())
}

/// Print all discovered subagent profiles (built-in + auto-discovered global/project `agents/*.md`).
pub(crate) fn agents(fs: &dyn WorkspaceFs, cwd: &Path) -> Result {
    let profiles = load_subagent_profiles(fs, cwd)?;
    if profiles.is_empty() {
        out("no agents\n");
        return Ok(());
    }
    out("NAME\tSOURCE\tMODE\tMODEL\tTHINK\tDESCRIPTION\n");
    for p in profiles.values() {
        let mode = if p.read_only { "ro" } else { "rw" };
        let model = p.model.as_deref().unwrap_or("-");
        let think = p.thinking.map_or("-", |t| t.as_str());
        out(&format!(
            "{}\t{}\t{mode}\t{model}\t{think}\t{}\n",
            p.name, p.source, p.description
        ));
    }
    Ok(())
}

/// Print all discovered `SKILL.md` skills across global and project skill directories.
pub(crate) fn skills(fs: &dyn WorkspaceFs, cwd: &Path) -> Result {
    let list = discover_skills(fs, cwd)?;
    if list.is_empty() {
        out("no skills discovered\n");
        return Ok(());
    }
    out("NAME\tSOURCE\tPATH\tDESCRIPTION\n");
    for s in list.values() {
        out(&format!(
            "{}\t{}\t{}\t{}\n",
            s.name,
            s.source,
            s.path.display(),
            s.description
        ));
    }
    Ok(())
}

/// Print all discovered custom slash commands across global and project `commands/*.md`.
pub(crate) fn custom_commands(fs: &dyn WorkspaceFs, cwd: &Path) -> Result {
    let list = discover_custom_commands(fs, cwd)?;
    if list.is_empty() {
        out("no custom commands discovered\n");
        return Ok(());
    }
    out("COMMAND\tSOURCE\tDESCRIPTION\n");
    for c in list.values() {
        out(&format!("/{}\t{}\t{}\n", c.name, c.source, c.description));
    }
    Ok(())
}

fn kind_name(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenaiChat => "openai_chat",
        ProviderKind::OpenaiResponses => "openai_responses",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::AgyAcp => "agy_acp",
    }
}

/// Render the resolved configuration; `Err` lists every key that cannot be resolved.
pub(crate) fn render_check(path: &Path, cfg: &Config) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut problems = Vec::new();
    let _ = writeln!(text, "config: {} (valid)", path.display());
    let _ = writeln!(text, "thinking: {}", cfg.thinking.as_str());
    // Imported providers (omp's models.yml) may lack a key; that only matters when a role,
    // the cycle or a fallback chain reaches one of their models.
    let reached: BTreeSet<&str> = cfg
        .llm
        .roles
        .values()
        .chain(&cfg.llm.cycle_order)
        .chain(cfg.llm.fallback_chains.keys())
        .chain(cfg.llm.fallback_chains.values().flatten())
        .filter_map(|key| cfg.llm.presets.get(key))
        .map(|preset| preset.provider.as_str())
        .collect();
    text.push_str("providers:\n");
    for (name, entry) in &cfg.llm.providers {
        let key = match provider_key_report(entry) {
            Ok(report) => report,
            Err(why) if reached.contains(name.as_str()) => {
                problems.push(format!("provider `{name}`: {why}"));
                format!("UNRESOLVED ({why})")
            }
            Err(why) => format!("unavailable ({why}); unused"),
        };
        let _ = writeln!(
            text,
            "  {name}: {} {} key={key}",
            kind_name(entry.kind),
            entry.effective_base_url()
        );
    }
    text.push_str("models:\n");
    for (alias, m) in &cfg.llm.presets {
        let think = m
            .thinking
            .map_or_else(String::new, |t| format!(" think={}", t.as_str()));
        let target = if *alias == format!("{}/{}", m.provider, m.id) {
            String::new()
        } else {
            format!(": {}/{}", m.provider, m.id)
        };
        let _ = writeln!(
            text,
            "  {alias}{target} ctx={}{think}",
            m.context_window.unwrap_or_default()
        );
    }
    text.push_str("roles:\n");
    let mut roles: Vec<&str> = cfg.llm.roles.keys().map(String::as_str).collect();
    if !cfg.llm.roles.contains_key("fast") {
        roles.push("fast");
    }
    for role in roles {
        let configured = cfg.llm.roles.contains_key(role);
        match resolve_alias(&cfg.llm, Some(role)) {
            Ok(alias) => {
                let note = if configured {
                    ""
                } else {
                    " (not configured; falls back to default)"
                };
                let _ = writeln!(text, "  {role} -> {alias}{note}");
            }
            Err(e) => {
                problems.push(format!("role `{role}`: {e}"));
                let _ = writeln!(text, "  {role} -> UNRESOLVED ({e})");
            }
        }
    }
    if !cfg.llm.fallback_chains.is_empty() {
        text.push_str("fallback:\n");
        for (model, chain) in &cfg.llm.fallback_chains {
            let _ = writeln!(text, "  {model} -> {}", chain.join(" -> "));
        }
    }
    if !cfg.llm.cycle_order.is_empty() {
        let _ = writeln!(text, "cycle: {}", cfg.llm.cycle_order.join(" -> "));
    }
    (text, problems)
}

/// Print the check report; fails when a provider key cannot be resolved.
pub(crate) fn config_check(path: &Path, cfg: &Config) -> Result {
    let (text, problems) = render_check(path, cfg);
    out(&text);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(HrnError::Config(problems.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> SessionMeta {
        SessionMeta {
            pinned: false,
            id: id.into(),
            title: "t".into(),
            cwd: "/w".into(),
            branch: None,
            head_node_id: None,
            model: None,
            thinking: hrn_kernel::ThinkingLevel::Medium,
            goal: None,
            permission_mode: hrn_kernel::PermissionMode::Default,
            parent_session_id: None,
            parent_node_id: None,
            status: hrn_kernel::AgentStatus::Idle,
            title_source: None,
            exit_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn deleting_a_session_needs_a_yes_unless_forced() {
        let store = hrn_edge_store::SqliteStore::open_in_memory().expect("store");
        for id in ["s1", "s2", "s3"] {
            store.create_session(&session(id)).expect("create");
        }
        let refused = |_: &str| false;
        let approved = |_: &str| true;
        delete_session(&store, "s1", false, &refused).expect("kept");
        assert!(store.get_session("s1").expect("read").is_some());
        delete_session(&store, "s1", false, &approved).expect("deleted");
        assert!(store.get_session("s1").expect("read").is_none());
        delete_session(&store, "s2", true, &refused).expect("--force skips the question");
        assert!(delete_session(&store, "gone", true, &refused).is_err());
        rename_session(&store, "s3", "renamed").expect("renamed");
        assert!(rename_session(&store, "s3", "  ").is_err());
        pin_session(&store, "s3", true).expect("pinned");
        assert_eq!(store.pinned_sessions().expect("pins"), ["s3"]);
    }

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00:00");
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn check_reports_fast_fallback_and_hides_keys() {
        let cfg = Config::parse(
            r#"
[[providers]]
name = "local"
kind = "openai_chat"

[models.m]
provider = "local"
id = "model-x"
context_window = 8192

[roles]
default = "m"
"#,
        )
        .expect("valid");
        let (text, problems) = render_check(Path::new("c.toml"), &cfg);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(text.contains("default -> m"));
        assert!(text.contains("fast -> m (not configured; falls back to default)"));
        assert!(text.contains("keyless"));
    }

    #[test]
    fn check_flags_an_unset_key_variable() {
        let cfg = Config::parse(
            r#"
[[providers]]
name = "p"
kind = "anthropic"
api_key_env = "HRN_TEST_SURELY_UNSET_KEY"

[models.m]
provider = "p"
id = "x"
context_window = 1000

[roles]
default = "m"
"#,
        )
        .expect("valid");
        let (_, problems) = render_check(Path::new("c.toml"), &cfg);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("HRN_TEST_SURELY_UNSET_KEY"));
    }
}
