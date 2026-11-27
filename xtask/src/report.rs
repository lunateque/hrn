//! The single place xtask prints: everything goes to stderr, stdout stays clean for `check`'s
//! one-line verdict.

use std::io::Write;

/// Print one line to stderr. Write errors are dropped: a closed stderr must not change the exit
/// status of a command that has already decided.
pub(crate) fn line(text: &str) {
    let stderr = std::io::stderr();
    let mut sink = stderr.lock();
    let _ = writeln!(sink, "{text}");
}

/// Print one line to stdout (the verdict of `check`).
pub(crate) fn out(text: &str) {
    let stdout = std::io::stdout();
    let mut sink = stdout.lock();
    let _ = writeln!(sink, "{text}");
}
