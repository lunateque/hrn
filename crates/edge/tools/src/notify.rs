//! Output matching for `bash`: `notify` lines delivered as follow-ups while a job or service
//! runs, and `ready.log` readiness over a service's accumulated output.

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::Lossy;
use hrn_kernel::{HrnError, Inbox, InboxItem, Result};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Matches within this window of the first go out as one notice.
const BATCH_WINDOW: Duration = Duration::from_millis(200);
/// Notices one job or service may send; the last one says notify is off.
const MAX_NOTICES: usize = 30;
/// Lines carried by one notice; further matches are only counted.
const MAX_LINES: usize = 50;
/// Longest line kept in a notice.
const MAX_LINE_CHARS: usize = 400;

/// A compiled Rust-syntax regex over text.
pub struct OutputRegex {
    matcher: RegexMatcher,
    multi_line: bool,
}

impl OutputRegex {
    /// A regex tested against single lines.
    pub fn per_line(pattern: &str, what: &str) -> Result<Self> {
        Self::build(&RegexMatcherBuilder::new(), pattern, what, false)
    }

    /// A regex over a whole output: `^` and `$` match at line boundaries and a match may span
    /// lines.
    pub fn whole(pattern: &str, what: &str) -> Result<Self> {
        let mut builder = RegexMatcherBuilder::new();
        builder.multi_line(true);
        Self::build(&builder, pattern, what, true)
    }

    fn build(
        builder: &RegexMatcherBuilder,
        pattern: &str,
        what: &str,
        multi_line: bool,
    ) -> Result<Self> {
        let matcher = builder
            .build(pattern)
            .map_err(|e| HrnError::Tool(format!("Invalid {what} regex: {e}")))?;
        Ok(Self {
            matcher,
            multi_line,
        })
    }

    /// The non-blank lines of `text` that match.
    pub fn matching_lines(&self, text: &str) -> Result<Vec<String>> {
        let mut found = Vec::new();
        SearcherBuilder::new()
            .build()
            .search_slice(
                &self.matcher,
                text.as_bytes(),
                Lossy(|_, line| {
                    let line = line.trim_end_matches(['\r', '\n']);
                    if !line.trim().is_empty() {
                        found.push(line.to_owned());
                    }
                    Ok(true)
                }),
            )
            .map_err(|e| HrnError::Tool(format!("matching output failed: {e}")))?;
        Ok(found)
    }

    /// Whether `text` matches anywhere.
    pub fn is_match(&self, text: &str) -> Result<bool> {
        let mut hit = false;
        SearcherBuilder::new()
            .multi_line(self.multi_line)
            .build()
            .search_slice(
                &self.matcher,
                text.as_bytes(),
                Lossy(|_, _| {
                    hit = true;
                    Ok(false)
                }),
            )
            .map_err(|e| HrnError::Tool(format!("matching output failed: {e}")))?;
        Ok(hit)
    }
}

#[derive(Default)]
struct Pending {
    lines: Vec<String>,
    omitted: usize,
    armed: bool,
    sent: usize,
    off: bool,
    running: bool,
}

/// Turns matching output lines into inbox follow-ups for one job or service, batched and
/// rate-limited: after `MAX_NOTICES` notices it switches off and the last one says so.
pub struct Notifier {
    regex: OutputRegex,
    inbox: Arc<Inbox>,
    session: String,
    id: String,
    pending: Arc<Mutex<Pending>>,
}

impl Notifier {
    /// A notifier for job or service `id` of `session`, matching with `regex`.
    pub fn new(regex: OutputRegex, inbox: Arc<Inbox>, session: &str, id: &str) -> Self {
        Self {
            regex,
            inbox,
            session: session.to_owned(),
            id: id.to_owned(),
            pending: Arc::new(Mutex::new(Pending {
                running: true,
                ..Pending::default()
            })),
        }
    }

    /// Offer complete output lines; the matching ones are queued for the next notice. Needs a
    /// tokio runtime (the batch window is a timer).
    pub fn feed(&self, text: &str) -> Result<()> {
        let lines = self.regex.matching_lines(text)?;
        if lines.is_empty() {
            return Ok(());
        }
        let mut p = lock(&self.pending);
        if p.off {
            return Ok(());
        }
        for line in lines {
            if p.lines.len() < MAX_LINES {
                p.lines.push(line.chars().take(MAX_LINE_CHARS).collect());
            } else {
                p.omitted += 1;
            }
        }
        if !p.armed {
            p.armed = true;
            let (pending, inbox, session, id) = (
                Arc::clone(&self.pending),
                Arc::clone(&self.inbox),
                self.session.clone(),
                self.id.clone(),
            );
            drop(p);
            drop(tokio::spawn(async move {
                tokio::time::sleep(BATCH_WINDOW).await;
                send(&pending, &inbox, &session, &id);
            }));
        }
        Ok(())
    }

    /// The process ended. A service's unsent matches go out now, ahead of its exit notice; a
    /// job's are dropped, its result carries the output.
    pub fn finish(&self, flush: bool) {
        lock(&self.pending).running = false;
        if flush {
            send(&self.pending, &self.inbox, &self.session, &self.id);
        } else {
            lock(&self.pending).off = true;
        }
    }

    /// Stop notifying and say why (matching failed).
    pub fn fail(&self, reason: &str) {
        lock(&self.pending).off = true;
        self.inbox.push(
            &self.session,
            InboxItem {
                source: format!("{}#notify", self.id),
                text: format!(
                    "<bash-notify id=\"{}\">\nnotify is off: {reason}\n</bash-notify>",
                    self.id
                ),
                summary: format!("{} notify off", self.id),
            },
        );
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn send(pending: &Mutex<Pending>, inbox: &Inbox, session: &str, id: &str) {
    let mut p = lock(pending);
    p.armed = false;
    if p.lines.is_empty() || p.off {
        return;
    }
    p.sent += 1;
    let last = p.sent >= MAX_NOTICES;
    p.off = last;
    let count = p.lines.len() + p.omitted;
    let mut text = format!(
        "<bash-notify id=\"{id}\" running=\"{}\">\n{}",
        p.running,
        p.lines.join("\n")
    );
    if p.omitted > 0 {
        let _ = write!(text, "\n… {} more matching lines not shown", p.omitted);
    }
    if last {
        let _ = write!(
            text,
            "\n[notify is now off: {MAX_NOTICES} notices sent. The command keeps running; read proc://{id} for its output.]"
        );
    }
    text.push_str("\n</bash-notify>");
    p.lines.clear();
    p.omitted = 0;
    drop(p);
    inbox.push(
        session,
        InboxItem {
            source: format!("{id}#notify"),
            text,
            summary: format!(
                "{id}: {count} matching line{}",
                if count == 1 { "" } else { "s" }
            ),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_line_regex_returns_matching_non_blank_lines() {
        let re = OutputRegex::per_line("^(ERROR|warn)", "notify").unwrap();
        let lines = re
            .matching_lines("ok\nERROR boom\r\n\nwarn: x\nnot ERROR\n")
            .unwrap();
        assert_eq!(lines, ["ERROR boom", "warn: x"]);
        assert!(OutputRegex::per_line("(", "notify").is_err());
    }

    #[test]
    fn whole_output_regex_anchors_at_lines_and_spans_them() {
        let re = OutputRegex::whole("^listening on \\d+$", "ready.log").unwrap();
        assert!(re.is_match("boot\nlistening on 8080\n").unwrap());
        assert!(re.is_match("boot\nlistening on 8080").unwrap());
        assert!(!re.is_match("boot\nstill listening on 8080 soon").unwrap());
        let span = OutputRegex::whole("a\\nb", "ready.log").unwrap();
        assert!(span.is_match("xa\nbx").unwrap());
    }

    #[tokio::test]
    async fn matches_batch_into_one_notice_and_switch_off_after_the_limit() {
        let inbox = Arc::new(Inbox::default());
        let notifier = Notifier::new(
            OutputRegex::per_line("hit", "notify").unwrap(),
            Arc::clone(&inbox),
            "s",
            "job-1",
        );
        notifier.feed("hit 1\nmiss\nhit 2\n").unwrap();
        notifier.feed("hit 3\n").unwrap();
        tokio::time::sleep(BATCH_WINDOW * 2).await;
        let items = inbox.take("s");
        assert_eq!(items.len(), 1);
        assert!(
            items[0].text.contains("hit 1\nhit 2\nhit 3"),
            "{}",
            items[0].text
        );
        assert_eq!(items[0].source, "job-1#notify");

        for _ in 0..MAX_NOTICES + 2 {
            notifier.feed("hit again\n").unwrap();
            tokio::time::sleep(BATCH_WINDOW * 2).await;
        }
        let items = inbox.take("s");
        assert_eq!(items.len(), MAX_NOTICES - 1);
        assert!(items.last().unwrap().text.contains("notify is now off"));
    }

    #[tokio::test]
    async fn a_services_pending_matches_flush_at_exit_and_a_jobs_are_dropped() {
        let inbox = Arc::new(Inbox::default());
        let service = Notifier::new(
            OutputRegex::per_line("hit", "notify").unwrap(),
            Arc::clone(&inbox),
            "s",
            "web",
        );
        service.feed("hit\n").unwrap();
        service.finish(true);
        assert_eq!(inbox.take("s").len(), 1);
        let job = Notifier::new(
            OutputRegex::per_line("hit", "notify").unwrap(),
            Arc::clone(&inbox),
            "s",
            "job-1",
        );
        job.feed("hit\n").unwrap();
        job.finish(false);
        tokio::time::sleep(BATCH_WINDOW * 2).await;
        assert!(inbox.take("s").is_empty());
    }
}
