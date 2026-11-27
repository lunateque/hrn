//! Linear-time Thompson NFA regex engine for the `pattern` attribute, matched against the whole
//! value: literals, `.`, classes (`[a-z]`, `[^x]`, `\d \w \s`), groups, `|`, `? * +`, and `{n,m}`.
//! Zero backtracking (`O(|states| * |text|)` worst-case time, immune to `ReDoS`). Unsupported or
//! over-expanded patterns fail to compile (`None`), matching browser `pattern` semantics.

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class(bool, Vec<Range>),
    Group(Vec<Vec<Node>>),
    Repeat(Box<Node>, usize, usize),
    Start,
    End,
}

/// An inclusive character range.
type Range = (char, char);

const DIGIT: &[Range] = &[('0', '9')];
const WORD: &[Range] = &[('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')];
const SPACE: &[Range] = &[(' ', ' '), ('\t', '\r')];

/// Upper bound on quantifier repetition expansion and compiled NFA size so adversarial patterns
/// like `a{1000000}` fail to compile cleanly instead of exhausting memory.
const MAX_REPEAT_EXPAND: usize = 256;
const MAX_NFA_STATES: usize = 4096;

#[derive(Debug)]
enum Inst {
    Char(char),
    Any,
    Class(bool, Vec<Range>),
    Start,
    End,
    Split(usize, usize),
    Jmp(usize),
    Match,
}

/// Whether `pattern` matches all of `text`; `None` when the pattern cannot be compiled.
pub(super) fn full_match(pattern: &str, text: &str) -> Option<bool> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut at = 0;
    let alts = alternatives(&chars, &mut at)?;
    if at != chars.len() {
        return None;
    }
    let mut prog = Vec::new();
    compile_group(&alts, &mut prog)?;
    prog.push(Inst::Match);
    let text: Vec<char> = text.chars().collect();
    Some(run_nfa(&prog, &text))
}

fn alternatives(p: &[char], at: &mut usize) -> Option<Vec<Vec<Node>>> {
    let mut alts = vec![Vec::new()];
    while let Some(&c) = p.get(*at) {
        let node = match c {
            ')' => break,
            '|' => {
                *at += 1;
                alts.push(Vec::new());
                continue;
            }
            '(' => {
                *at += 1;
                if p.get(*at..)
                    .is_some_and(|rest| rest.starts_with(&['?', ':']))
                {
                    *at += 2;
                }
                let inner = alternatives(p, at)?;
                if p.get(*at) != Some(&')') {
                    return None;
                }
                *at += 1;
                Node::Group(inner)
            }
            '[' => class(p, at)?,
            '.' => {
                *at += 1;
                Node::Any
            }
            '^' => {
                *at += 1;
                Node::Start
            }
            '$' => {
                *at += 1;
                Node::End
            }
            '*' | '+' | '?' | '{' => return None,
            '\\' => {
                *at += 1;
                escape(p, at)?
            }
            _ => {
                *at += 1;
                Node::Char(c)
            }
        };
        let node = quantified(p, at, node)?;
        alts.last_mut()?.push(node);
    }
    Some(alts)
}

fn quantified(p: &[char], at: &mut usize, node: Node) -> Option<Node> {
    let (min, max) = match p.get(*at) {
        Some('*') => (0, usize::MAX),
        Some('+') => (1, usize::MAX),
        Some('?') => (0, 1),
        Some('{') => {
            let rest = p.get(*at..)?;
            let close = rest.iter().position(|c| *c == '}')?;
            let body: String = rest.get(1..close)?.iter().collect();
            let (lo, hi) = match body.split_once(',') {
                Some((lo, "")) => (lo.parse().ok()?, usize::MAX),
                Some((lo, hi)) => (lo.parse().ok()?, hi.parse().ok()?),
                None => (body.parse().ok()?, body.parse().ok()?),
            };
            *at += close;
            (lo, hi)
        }
        _ => return Some(node),
    };
    *at += 1;
    if p.get(*at) == Some(&'?') {
        *at += 1; // lazy: identical language for whole-string matching
    }
    let finite_bound = if max == usize::MAX { min } else { max };
    (min <= max && finite_bound <= MAX_REPEAT_EXPAND)
        .then(|| Node::Repeat(Box::new(node), min, max))
}

/// The escape after a `\`: a class shorthand or a literal.
fn escape(p: &[char], at: &mut usize) -> Option<Node> {
    let c = *p.get(*at)?;
    *at += 1;
    Some(match c {
        'd' => Node::Class(false, DIGIT.to_vec()),
        'D' => Node::Class(true, DIGIT.to_vec()),
        'w' => Node::Class(false, WORD.to_vec()),
        'W' => Node::Class(true, WORD.to_vec()),
        's' => Node::Class(false, SPACE.to_vec()),
        'S' => Node::Class(true, SPACE.to_vec()),
        'n' => Node::Char('\n'),
        'r' => Node::Char('\r'),
        't' => Node::Char('\t'),
        'b' | 'B' | '1'..='9' => return None,
        other => Node::Char(other),
    })
}

fn class(p: &[char], at: &mut usize) -> Option<Node> {
    *at += 1;
    let negated = p.get(*at) == Some(&'^');
    if negated {
        *at += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let c = *p.get(*at)?;
        *at += 1;
        if c == ']' && !first {
            return Some(Node::Class(negated, ranges));
        }
        first = false;
        if c == '\\' {
            let e = *p.get(*at)?;
            *at += 1;
            match e {
                'd' => ranges.extend_from_slice(DIGIT),
                'w' => ranges.extend_from_slice(WORD),
                's' => ranges.extend_from_slice(SPACE),
                'n' => ranges.push(('\n', '\n')),
                'r' => ranges.push(('\r', '\r')),
                't' => ranges.push(('\t', '\t')),
                other => ranges.push((other, other)),
            }
            continue;
        }
        if p.get(*at) == Some(&'-')
            && let Some(&hi) = p.get(*at + 1).filter(|n| **n != ']')
        {
            if c > hi {
                return None;
            }
            *at += 2;
            ranges.push((c, hi));
        } else {
            ranges.push((c, c));
        }
    }
}

fn push_inst(prog: &mut Vec<Inst>, inst: Inst) -> Option<usize> {
    if prog.len() >= MAX_NFA_STATES {
        return None;
    }
    let idx = prog.len();
    prog.push(inst);
    Some(idx)
}

fn compile_seq(nodes: &[Node], prog: &mut Vec<Inst>) -> Option<()> {
    for node in nodes {
        compile_node(node, prog)?;
    }
    Some(())
}

fn compile_group(alts: &[Vec<Node>], prog: &mut Vec<Inst>) -> Option<()> {
    let Some((first, rest)) = alts.split_first() else {
        return Some(());
    };
    if rest.is_empty() {
        return compile_seq(first, prog);
    }
    let mut end_jmps = Vec::with_capacity(alts.len());
    for (idx, alt) in alts.iter().enumerate() {
        let is_last = idx + 1 == alts.len();
        if is_last {
            compile_seq(alt, prog)?;
        } else {
            let split_pc = push_inst(prog, Inst::Split(0, 0))?;
            let alt_start = prog.len();
            compile_seq(alt, prog)?;
            let jmp_pc = push_inst(prog, Inst::Jmp(0))?;
            end_jmps.push(jmp_pc);
            let next_start = prog.len();
            if let Some(slot) = prog.get_mut(split_pc) {
                *slot = Inst::Split(alt_start, next_start);
            }
        }
    }
    let after = prog.len();
    for jmp_pc in end_jmps {
        if let Some(slot) = prog.get_mut(jmp_pc) {
            *slot = Inst::Jmp(after);
        }
    }
    Some(())
}

fn compile_node(node: &Node, prog: &mut Vec<Inst>) -> Option<()> {
    match node {
        Node::Char(c) => {
            push_inst(prog, Inst::Char(*c))?;
        }
        Node::Any => {
            push_inst(prog, Inst::Any)?;
        }
        Node::Class(neg, ranges) => {
            push_inst(prog, Inst::Class(*neg, ranges.clone()))?;
        }
        Node::Start => {
            push_inst(prog, Inst::Start)?;
        }
        Node::End => {
            push_inst(prog, Inst::End)?;
        }
        Node::Group(alts) => compile_group(alts, prog)?,
        Node::Repeat(inner, min, max) => {
            for _ in 0..*min {
                compile_node(inner, prog)?;
            }
            if *max == usize::MAX {
                let split_pc = push_inst(prog, Inst::Split(0, 0))?;
                let body_start = prog.len();
                compile_node(inner, prog)?;
                push_inst(prog, Inst::Jmp(split_pc))?;
                let after = prog.len();
                if let Some(slot) = prog.get_mut(split_pc) {
                    *slot = Inst::Split(body_start, after);
                }
            } else {
                for _ in *min..*max {
                    let split_pc = push_inst(prog, Inst::Split(0, 0))?;
                    let body_start = prog.len();
                    compile_node(inner, prog)?;
                    let after = prog.len();
                    if let Some(slot) = prog.get_mut(split_pc) {
                        *slot = Inst::Split(body_start, after);
                    }
                }
            }
        }
    }
    Some(())
}

fn add_state(
    prog: &[Inst],
    start_pc: usize,
    pos: usize,
    len: usize,
    out: &mut Vec<usize>,
    seen: &mut [u32],
    epoch: u32,
) {
    let mut stack = vec![start_pc];
    while let Some(pc) = stack.pop() {
        let Some(mark) = seen.get_mut(pc) else {
            continue;
        };
        if *mark == epoch {
            continue;
        }
        *mark = epoch;
        match prog.get(pc) {
            Some(Inst::Jmp(target)) => stack.push(*target),
            Some(Inst::Split(a, b)) => {
                stack.push(*b);
                stack.push(*a);
            }
            Some(Inst::Start) => {
                if pos == 0 {
                    stack.push(pc + 1);
                }
            }
            Some(Inst::End) => {
                if pos == len {
                    stack.push(pc + 1);
                }
            }
            Some(Inst::Char(_) | Inst::Any | Inst::Class(_, _) | Inst::Match) => out.push(pc),
            None => {}
        }
    }
}

fn run_nfa(prog: &[Inst], text: &[char]) -> bool {
    let mut seen = vec![0u32; prog.len()];
    let mut epoch = 1u32;
    let mut current = Vec::new();
    let mut next = Vec::new();
    add_state(prog, 0, 0, text.len(), &mut current, &mut seen, epoch);

    for (pos, &c) in text.iter().enumerate() {
        epoch = epoch.wrapping_add(1);
        if epoch == 0 {
            seen.fill(0);
            epoch = 1;
        }
        next.clear();
        for &pc in &current {
            let hit = match prog.get(pc) {
                Some(Inst::Char(want)) => c == *want,
                Some(Inst::Any) => c != '\n',
                Some(Inst::Class(negated, ranges)) => {
                    ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&c)) != *negated
                }
                _ => false,
            };
            if hit {
                add_state(
                    prog,
                    pc + 1,
                    pos + 1,
                    text.len(),
                    &mut next,
                    &mut seen,
                    epoch,
                );
            }
        }
        std::mem::swap(&mut current, &mut next);
        if current.is_empty() {
            return false;
        }
    }
    current
        .iter()
        .any(|&pc| matches!(prog.get(pc), Some(Inst::Match)))
}

#[cfg(test)]
mod tests {
    use super::full_match;

    fn m(pattern: &str, text: &str) -> bool {
        full_match(pattern, text).expect("pattern compiles")
    }

    #[test]
    fn matches_the_whole_value() {
        assert!(m("[a-z]{3}-\\d+", "abc-42"));
        assert!(!m("[a-z]{3}-\\d+", "abc-42x"));
        assert!(!m("[a-z]{3}", "ab"));
        assert!(m("(foo|bar)?baz", "baz"));
        assert!(m("(foo|bar)?baz", "foobaz"));
        assert!(m("^\\w+@\\w+\\.com$", "me@site.com"));
        assert!(!m("[^0-9]+", "ab1"));
        assert!(m("a.*c", "abbbc"));
        assert!(m("(a*)*b", "aaab"));
        assert!(m("a\\r\\nb", "a\r\nb"));
    }

    #[test]
    fn unsupported_or_broken_patterns_do_not_compile() {
        assert_eq!(full_match("(abc", "abc"), None);
        assert_eq!(full_match("a\\bc", "ac"), None);
        assert_eq!(full_match("*a", "a"), None);
        assert_eq!(full_match("[z-a]", "m"), None);
        assert_eq!(full_match("a{999999}", "a"), None);
    }

    #[test]
    fn pathological_nested_quantifiers_terminate_in_linear_time() {
        let evil = format!("{}!", "a".repeat(500));
        assert!(!m("(a*)*b", &evil));
        assert!(!m("(a+)+b", &evil));
        assert!(!m("(a|aa)+b", &evil));
    }
}
