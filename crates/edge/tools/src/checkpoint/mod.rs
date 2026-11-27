//! Working-tree snapshots for `checkpoint`, `rewind` and `hrn checkpoints`.
//!
//! One shadow git repository per working directory lives under the checkpoint root
//! (`<root>/<hash of the directory>`), apart from any repository the directory itself is. A
//! snapshot stages the whole tree (honouring `.gitignore`, `info/exclude` and the configured
//! excludes) and commits it, so a snapshot costs only the files that changed since the last one
//! and works in directories that are not git repositories. Restoring is a `read-tree` of the
//! snapshot's tree, so files created since are removed too; the state it replaces is snapshotted
//! first, which makes every restore undoable.

mod git;

use git::Git;
use hrn_kernel::{HrnError, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hex digits of a snapshot id.
const ID_LEN: usize = 12;
const KIND_KEY: &str = "hrn-kind";
const SESSION_KEY: &str = "hrn-session";
const TURN_KEY: &str = "hrn-turn";
/// Separates fields and records of `git log` output.
const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

/// Why a snapshot was taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Before a turn's first file-changing tool call.
    Auto,
    /// The `checkpoint` tool.
    Explicit,
    /// The state a restore replaced.
    Restore,
}

impl Kind {
    /// The word stored with the snapshot and shown in listings.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Explicit => "explicit",
            Self::Restore => "restore",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        [Self::Auto, Self::Explicit, Self::Restore]
            .into_iter()
            .find(|kind| kind.as_str() == word)
    }
}

/// One stored snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Short id, accepted by [`Checkpoints::restore`] and [`Checkpoints::find`].
    pub id: String,
    /// Local time it was taken, `YYYY-MM-DD HH:MM:SS`.
    pub when: String,
    /// Why it was taken.
    pub kind: Kind,
    /// Session that took it, if any.
    pub session: Option<String>,
    /// Turn that took it, if any.
    pub turn: Option<String>,
    /// What the snapshot is for (the checkpoint goal, or a note).
    pub label: String,
}

/// One file a restore changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// `A` (put back), `M` (rewritten) or `D` (removed).
    pub status: char,
    /// Path relative to the working directory.
    pub path: String,
}

/// What a restore did.
#[derive(Clone, Debug)]
pub struct Restored {
    /// The state the restore replaced; restoring it undoes the restore.
    pub undo: String,
    /// The files that differ now.
    pub changes: Vec<Change>,
}

/// The snapshots of one working directory.
pub struct Checkpoints {
    git_dir: PathBuf,
    work_tree: PathBuf,
    timeout: Duration,
}

/// FNV-1a over the path: a stable directory name (std's hashers change between releases).
fn fingerprint(path: &str) -> u64 {
    path.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn check_session(session: &str) -> Result<&str> {
    if !session.is_empty()
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Ok(session)
    } else {
        Err(HrnError::Tool(format!(
            "checkpoint: `{session}` is not a session id"
        )))
    }
}

impl Checkpoints {
    /// The snapshots of `work_tree` under `root`. With `excludes` (gitignore patterns for what
    /// never gets snapshotted) the shadow repository is created on first use and keeps those
    /// patterns; with `None` an absent repository stays absent (it has no snapshots) and an
    /// existing one keeps the patterns its writer set. `timeout` bounds each git call.
    ///
    /// # Errors
    ///
    /// `work_tree` is missing, a home or root directory (far too large to snapshot), or the
    /// repository cannot be created.
    pub fn open(
        root: &Path,
        work_tree: &Path,
        excludes: Option<&[String]>,
        timeout: Duration,
    ) -> Result<Self> {
        let work_tree = std::fs::canonicalize(work_tree)
            .map_err(|e| HrnError::Io(format!("checkpoint: `{}`: {e}", work_tree.display())))?;
        let is_home = std::env::home_dir()
            .and_then(|home| std::fs::canonicalize(home).ok())
            .is_some_and(|home| home == work_tree);
        if is_home || work_tree.parent().is_none() {
            return Err(HrnError::Tool(format!(
                "checkpoint: refusing to snapshot `{}`: a home or root directory is too large; \
                 work in a project directory",
                work_tree.display()
            )));
        }
        let key = work_tree.to_string_lossy();
        let key = if cfg!(windows) {
            key.to_lowercase()
        } else {
            key.into_owned()
        };
        let this = Self {
            git_dir: root.join(format!("{:016x}", fingerprint(&key))),
            work_tree,
            timeout,
        };
        if let Some(excludes) = excludes {
            this.init(excludes)?;
        }
        Ok(this)
    }

    fn git(&self) -> Git<'_> {
        Git {
            dir: &self.git_dir,
            work_tree: &self.work_tree,
            timeout: self.timeout,
        }
    }

    fn io_err(&self, what: &str, err: &std::io::Error) -> HrnError {
        HrnError::Io(format!(
            "checkpoint: {what} `{}`: {err}",
            self.git_dir.display()
        ))
    }

    fn init(&self, excludes: &[String]) -> Result {
        if !self.git_dir.join("HEAD").is_file() {
            std::fs::create_dir_all(&self.git_dir).map_err(|e| self.io_err("create", &e))?;
            self.git().run(&["init", "-q", "--template="])?;
            // Whatever the user's git config says (CRLF conversion, signing, hooks), a
            // snapshot is byte-exact and silent.
            let hooks = self.git_dir.join("no-hooks");
            let config = format!(
                "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n\
                 \tautocrlf = false\n\tsafecrlf = false\n\tlongpaths = true\n\tquotepath = false\n\
                 \tfsmonitor = false\n\tlogAllRefUpdates = false\n\thooksPath = {}\n\
                 [user]\n\tname = hrn\n\temail = hrn@localhost\n[commit]\n\tgpgsign = false\n",
                hooks.to_string_lossy().replace('\\', "/")
            );
            std::fs::write(self.git_dir.join("config"), config)
                .map_err(|e| self.io_err("write config in", &e))?;
        }
        let info = self.git_dir.join("info");
        std::fs::create_dir_all(&info).map_err(|e| self.io_err("create", &e))?;
        let wanted = excludes.join("\n") + "\n";
        let file = info.join("exclude");
        if std::fs::read_to_string(&file).ok().as_deref() != Some(wanted.as_str()) {
            std::fs::write(&file, wanted).map_err(|e| self.io_err("write excludes in", &e))?;
        }
        Ok(())
    }

    /// Snapshot the working tree now and return the snapshot's id.
    ///
    /// # Errors
    ///
    /// `git` is missing, ran too long, or refused.
    pub fn snapshot(
        &self,
        kind: Kind,
        label: &str,
        session: Option<&str>,
        turn: Option<&str>,
    ) -> Result<String> {
        let label = one_line(label);
        let mut lines = vec![
            if label.is_empty() { "snapshot" } else { &label }.to_owned(),
            String::new(),
            format!("{KIND_KEY}: {}", kind.as_str()),
        ];
        for (key, value) in [(SESSION_KEY, session), (TURN_KEY, turn)] {
            lines.extend(value.map(|value| format!("{key}: {value}")));
        }
        let message = lines.join("\n");
        let git = self.git();
        git.run(&["add", "--all", "--", "."])?;
        git.run(&[
            "commit",
            "-q",
            "--allow-empty",
            "--no-verify",
            "--cleanup=verbatim",
            "-m",
            &message,
        ])?;
        let head = git.run(&["rev-parse", "HEAD"])?.text();
        Ok(head.trim().chars().take(ID_LEN).collect())
    }

    /// The newest `limit` snapshots (all when `None`), newest first.
    ///
    /// # Errors
    ///
    /// `git` is missing or refused.
    pub fn list(&self, limit: Option<usize>) -> Result<Vec<Snapshot>> {
        let git = self.git();
        if git
            .run_unchecked(&["rev-parse", "--verify", "--quiet", "HEAD"])?
            .status
            != Some(0)
        {
            return Ok(Vec::new());
        }
        let format = format!("--format=%H{FIELD}%cd{FIELD}%B{RECORD}");
        let mut args = vec!["log", "--date=format-local:%Y-%m-%d %H:%M:%S", &format];
        let max = limit.map(|limit| format!("--max-count={limit}"));
        args.extend(max.as_deref());
        let printed = git.run(&args)?.text();
        Ok(printed.split(RECORD).filter_map(parse_record).collect())
    }

    /// The snapshot whose id starts with `prefix`, or `None` when nothing matches (including a
    /// `prefix` that is no id at all).
    ///
    /// # Errors
    ///
    /// `prefix` matches more than one snapshot.
    pub fn lookup(&self, prefix: &str) -> Result<Option<Snapshot>> {
        if prefix.len() < 4 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let prefix = prefix.to_ascii_lowercase();
        let mut matches: Vec<Snapshot> = self
            .list(None)?
            .into_iter()
            .filter(|snap| snap.id.starts_with(&prefix))
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err(HrnError::Tool(format!(
                "checkpoint: `{prefix}` matches {} snapshots; use more digits",
                matches.len()
            ))),
        }
    }

    /// The snapshot whose id starts with `prefix`, or an error when there is none.
    ///
    /// # Errors
    ///
    /// Whatever [`Checkpoints::lookup`] reports, plus: `prefix` names no snapshot.
    pub fn find(&self, prefix: &str) -> Result<Snapshot> {
        self.lookup(prefix)?.ok_or_else(|| {
            HrnError::Tool(format!(
                "checkpoint: no snapshot `{prefix}` for this directory (see `hrn checkpoints list`)"
            ))
        })
    }

    /// Put the working tree back to snapshot `id`: files changed since are rewritten, files
    /// created since are removed, files the snapshots leave out (ignored, excluded) stay.
    ///
    /// # Errors
    ///
    /// `id` names no snapshot, or `git` refused.
    pub fn restore(&self, id: &str, session: Option<&str>, turn: Option<&str>) -> Result<Restored> {
        let target = self.find(id)?;
        let undo = self.snapshot(
            Kind::Restore,
            &format!("before restoring {}", target.id),
            session,
            turn,
        )?;
        let git = self.git();
        let diff = git
            .run(&[
                "diff-tree",
                "-r",
                "-z",
                "--name-status",
                "--no-renames",
                &undo,
                &target.id,
            ])?
            .text();
        git.run(&["read-tree", "--reset", "-u", &target.id])?;
        let mut fields = diff.split('\0');
        let mut changes = Vec::new();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            let status = match status {
                "A" => 'A',
                "D" => 'D',
                _ => 'M',
            };
            changes.push(Change {
                status,
                path: path.to_owned(),
            });
        }
        Ok(Restored { undo, changes })
    }

    fn active_ref(session: &str) -> Result<String> {
        Ok(format!("refs/hrn/active/{}", check_session(session)?))
    }

    /// The session's active explicit checkpoint, if one is waiting for its `rewind`.
    ///
    /// # Errors
    ///
    /// `session` is not an id, or `git` refused.
    pub fn active(&self, session: &str) -> Result<Option<String>> {
        let printed = self.git().run_unchecked(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &Self::active_ref(session)?,
        ])?;
        Ok((printed.status == Some(0))
            .then(|| printed.text().trim().chars().take(ID_LEN).collect()))
    }

    /// Mark snapshot `id` as the session's active checkpoint.
    ///
    /// # Errors
    ///
    /// `session` is not an id, `id` names no snapshot, or `git` refused.
    pub fn set_active(&self, session: &str, id: &str) -> Result {
        let target = self.find(id)?;
        self.git()
            .run(&["update-ref", &Self::active_ref(session)?, &target.id])?;
        Ok(())
    }

    /// Forget the session's active checkpoint.
    ///
    /// # Errors
    ///
    /// `session` is not an id, or `git` refused.
    pub fn clear_active(&self, session: &str) -> Result {
        self.git()
            .run(&["update-ref", "-d", &Self::active_ref(session)?])?;
        Ok(())
    }
}

/// One `git log` record, or `None` for a commit that is not a snapshot.
fn parse_record(record: &str) -> Option<Snapshot> {
    let mut fields = record.trim_start().splitn(3, FIELD);
    let id = fields.next()?;
    let when = fields.next()?;
    let message = fields.next()?;
    let (label, trailers) = message.split_once("\n\n").unwrap_or((message, ""));
    let trailer = |key: &str| {
        trailers
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
            .map(str::to_owned)
    };
    Some(Snapshot {
        id: id.chars().take(ID_LEN).collect(),
        when: when.to_owned(),
        kind: Kind::parse(&trailer(KIND_KEY)?)?,
        session: trailer(SESSION_KEY),
        turn: trailer(TURN_KEY),
        label: label.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests;
