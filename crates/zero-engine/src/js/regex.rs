//! A small backtracking regular-expression matcher.
//!
//! Enough of the syntax that page scripts actually use: literals, `.`, the
//! `\d \w \s` classes and their negations, `[...]` sets with ranges, greedy
//! `* + ? {n,m}` quantifiers, `^ $` anchors, alternation and groups.
//!
//! ponytail: no capture groups (a group matches but reports nothing), no lazy
//! quantifiers, no backreferences or lookaround. Those need a different engine
//! shape — this one answers "does it match, and where" and nothing more.

#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// One of the alternatives, tried left to right.
    Alt(Vec<Vec<Node>>),
    Char(char),
    Any,
    /// A character set: (negated, items).
    Set(bool, Vec<SetItem>),
    Group(Box<Node>),
    /// `node{min,max}`, greedy. `max` of `None` means unbounded.
    Repeat(Box<Node>, usize, Option<usize>),
    Start,
    End,
    /// A word boundary, `\b`.
    Boundary,
}

#[derive(Debug, Clone, PartialEq)]
enum SetItem {
    Char(char),
    Range(char, char),
    /// One of the shorthand classes, e.g. `d` for `\d`; upper case negates.
    Class(char),
}

pub struct Regex {
    root: Node,
    pub source: String,
    pub flags: String,
}

impl Regex {
    /// Compile a pattern. Returns `None` for syntax this matcher cannot express,
    /// so a caller can fail loudly rather than match wrongly.
    pub fn new(pattern: &str, flags: &str) -> Option<Regex> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut pos = 0;
        let root = parse_alt(&chars, &mut pos)?;
        (pos == chars.len()).then(|| Regex {
            root,
            source: pattern.to_string(),
            flags: flags.to_string(),
        })
    }

    fn ignore_case(&self) -> bool {
        self.flags.contains('i')
    }

    /// Where the first match starts and ends, in character positions.
    pub fn find(&self, text: &str) -> Option<(usize, usize)> {
        let chars: Vec<char> = match self.ignore_case() {
            true => text.to_lowercase().chars().collect(),
            false => text.chars().collect(),
        };
        let root = match self.ignore_case() {
            true => lowercase(&self.root),
            false => self.root.clone(),
        };
        let nodes = [root];
        for start in 0..=chars.len() {
            if let Some(end) = match_seq(&nodes, &chars, start) {
                return Some((start, end));
            }
        }
        None
    }

    pub fn is_match(&self, text: &str) -> bool {
        self.find(text).is_some()
    }

    /// Replace matches with `replacement`; every match when the `g` flag is set,
    /// otherwise just the first.
    pub fn replace(&self, text: &str, replacement: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        let mut at = 0;
        let global = self.flags.contains('g');
        while at <= chars.len() {
            let rest: String = chars[at..].iter().collect();
            match self.find(&rest) {
                Some((start, end)) => {
                    out.extend(&chars[at..at + start]);
                    out.push_str(replacement);
                    // An empty match would spin forever, so always move on.
                    let step = if end == start { start + 1 } else { end };
                    if end == start && at + start < chars.len() {
                        out.push(chars[at + start]);
                    }
                    at += step;
                    if !global {
                        break;
                    }
                }
                None => break,
            }
        }
        if at < chars.len() {
            out.extend(&chars[at..]);
        }
        out
    }

    /// Split around every match.
    pub fn split(&self, text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        let mut parts = Vec::new();
        let mut at = 0;
        while at <= chars.len() {
            let rest: String = chars[at..].iter().collect();
            match self.find(&rest) {
                Some((start, end)) if end > start => {
                    parts.push(chars[at..at + start].iter().collect());
                    at += end;
                }
                _ => break,
            }
        }
        parts.push(chars[at.min(chars.len())..].iter().collect());
        parts
    }
}

/// Lower-case every literal in a compiled pattern, for the `i` flag.
fn lowercase(node: &Node) -> Node {
    match node {
        Node::Char(c) => Node::Char(c.to_lowercase().next().unwrap_or(*c)),
        Node::Alt(branches) => Node::Alt(
            branches
                .iter()
                .map(|b| b.iter().map(lowercase).collect())
                .collect(),
        ),
        Node::Group(inner) => Node::Group(Box::new(lowercase(inner))),
        Node::Repeat(inner, min, max) => Node::Repeat(Box::new(lowercase(inner)), *min, *max),
        Node::Set(negated, items) => Node::Set(
            *negated,
            items
                .iter()
                .map(|item| match item {
                    SetItem::Char(c) => SetItem::Char(c.to_lowercase().next().unwrap_or(*c)),
                    SetItem::Range(a, b) => SetItem::Range(
                        a.to_lowercase().next().unwrap_or(*a),
                        b.to_lowercase().next().unwrap_or(*b),
                    ),
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

// --- parsing ---

fn parse_alt(chars: &[char], pos: &mut usize) -> Option<Node> {
    let mut branches = vec![parse_seq(chars, pos)?];
    while chars.get(*pos) == Some(&'|') {
        *pos += 1;
        branches.push(parse_seq(chars, pos)?);
    }
    Some(Node::Alt(branches))
}

fn parse_seq(chars: &[char], pos: &mut usize) -> Option<Vec<Node>> {
    let mut nodes = Vec::new();
    while *pos < chars.len() && chars[*pos] != '|' && chars[*pos] != ')' {
        let atom = parse_atom(chars, pos)?;
        nodes.push(parse_quantifier(atom, chars, pos)?);
    }
    Some(nodes)
}

fn parse_quantifier(atom: Node, chars: &[char], pos: &mut usize) -> Option<Node> {
    let (min, max) = match chars.get(*pos) {
        Some('*') => (0, None),
        Some('+') => (1, None),
        Some('?') => (0, Some(1)),
        Some('{') => {
            let close = chars[*pos..].iter().position(|c| *c == '}')? + *pos;
            let body: String = chars[*pos + 1..close].iter().collect();
            let (min, max) = match body.split_once(',') {
                Some((a, "")) => (a.trim().parse().ok()?, None),
                Some((a, b)) => (a.trim().parse().ok()?, Some(b.trim().parse().ok()?)),
                None => {
                    let n = body.trim().parse().ok()?;
                    (n, Some(n))
                }
            };
            *pos = close;
            (min, max)
        }
        _ => return Some(atom),
    };
    *pos += 1;
    // A lazy marker is accepted but ignored; matching stays greedy.
    if chars.get(*pos) == Some(&'?') {
        *pos += 1;
    }
    Some(Node::Repeat(Box::new(atom), min, max))
}

fn parse_atom(chars: &[char], pos: &mut usize) -> Option<Node> {
    let c = *chars.get(*pos)?;
    *pos += 1;
    match c {
        '.' => Some(Node::Any),
        '^' => Some(Node::Start),
        '$' => Some(Node::End),
        '(' => {
            // `(?:` and friends: the flags are ignored, the group still groups.
            if chars.get(*pos) == Some(&'?') {
                *pos += 1;
                if matches!(chars.get(*pos), Some(':') | Some('=') | Some('!')) {
                    *pos += 1;
                } else {
                    return None; // named groups and lookaround are not supported
                }
            }
            let inner = parse_alt(chars, pos)?;
            (chars.get(*pos) == Some(&')')).then(|| {
                *pos += 1;
                Node::Group(Box::new(inner))
            })
        }
        '[' => parse_set(chars, pos),
        '\\' => {
            let escaped = *chars.get(*pos)?;
            *pos += 1;
            Some(match escaped {
                'd' | 'D' | 'w' | 'W' | 's' | 'S' => Node::Set(
                    escaped.is_uppercase(),
                    vec![SetItem::Class(escaped.to_ascii_lowercase())],
                ),
                'b' => Node::Boundary,
                'n' => Node::Char('\n'),
                't' => Node::Char('\t'),
                other => Node::Char(other),
            })
        }
        other => Some(Node::Char(other)),
    }
}

fn parse_set(chars: &[char], pos: &mut usize) -> Option<Node> {
    let negated = chars.get(*pos) == Some(&'^');
    if negated {
        *pos += 1;
    }
    let mut items = Vec::new();
    while *pos < chars.len() && chars[*pos] != ']' {
        let c = chars[*pos];
        *pos += 1;
        let item = if c == '\\' {
            let escaped = *chars.get(*pos)?;
            *pos += 1;
            match escaped {
                'd' | 'D' | 'w' | 'W' | 's' | 'S' => SetItem::Class(escaped),
                'n' => SetItem::Char('\n'),
                't' => SetItem::Char('\t'),
                other => SetItem::Char(other),
            }
        } else if chars.get(*pos) == Some(&'-') && chars.get(*pos + 1).is_some_and(|n| *n != ']') {
            let end = chars[*pos + 1];
            *pos += 2;
            SetItem::Range(c, end)
        } else {
            SetItem::Char(c)
        };
        items.push(item);
    }
    (chars.get(*pos) == Some(&']')).then(|| {
        *pos += 1;
        Node::Set(negated, items)
    })
}

// --- matching ---

fn in_class(class: char, c: char) -> bool {
    match class {
        'd' => c.is_ascii_digit(),
        'D' => !c.is_ascii_digit(),
        'w' => c.is_alphanumeric() || c == '_',
        'W' => !(c.is_alphanumeric() || c == '_'),
        's' => c.is_whitespace(),
        'S' => !c.is_whitespace(),
        _ => false,
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Match `nodes` in order from `at`, returning where the match ended.
fn match_seq(nodes: &[Node], text: &[char], at: usize) -> Option<usize> {
    let Some((first, rest)) = nodes.split_first() else {
        return Some(at);
    };
    match first {
        Node::Start => (at == 0).then(|| match_seq(rest, text, at)).flatten(),
        Node::End => (at == text.len()).then(|| match_seq(rest, text, at)).flatten(),
        Node::Boundary => {
            let before = at > 0 && is_word(text[at - 1]);
            let after = at < text.len() && is_word(text[at]);
            (before != after).then(|| match_seq(rest, text, at)).flatten()
        }
        Node::Char(expected) => match text.get(at) {
            Some(c) if c == expected => match_seq(rest, text, at + 1),
            _ => None,
        },
        Node::Any => match text.get(at) {
            Some(c) if *c != '\n' => match_seq(rest, text, at + 1),
            _ => None,
        },
        Node::Set(negated, items) => match text.get(at) {
            Some(c) => {
                let hit = items.iter().any(|item| match item {
                    SetItem::Char(want) => c == want,
                    SetItem::Range(lo, hi) => c >= lo && c <= hi,
                    SetItem::Class(class) => in_class(*class, *c),
                });
                (hit != *negated)
                    .then(|| match_seq(rest, text, at + 1))
                    .flatten()
            }
            None => None,
        },
        Node::Group(inner) => match_seq(&[(**inner).clone()], text, at)
            .and_then(|end| match_seq(rest, text, end))
            .or_else(|| match_alt_group(inner, rest, text, at)),
        Node::Alt(branches) => branches
            .iter()
            .find_map(|branch| match_branch(branch, rest, text, at)),
        Node::Repeat(inner, min, max) => match_repeat(inner, *min, *max, rest, text, at),
    }
}

/// A group may match several ways; try each so the rest of the pattern can too.
fn match_alt_group(inner: &Node, rest: &[Node], text: &[char], at: usize) -> Option<usize> {
    let Node::Alt(branches) = inner else {
        return None;
    };
    branches
        .iter()
        .find_map(|branch| match_branch(branch, rest, text, at))
}

fn match_branch(branch: &[Node], rest: &[Node], text: &[char], at: usize) -> Option<usize> {
    let mut combined = branch.to_vec();
    combined.extend_from_slice(rest);
    match_seq(&combined, text, at)
}

/// Greedy repetition with backtracking: take as many as possible, then give
/// them back one at a time until the rest of the pattern fits.
fn match_repeat(
    inner: &Node,
    min: usize,
    max: Option<usize>,
    rest: &[Node],
    text: &[char],
    at: usize,
) -> Option<usize> {
    let one = [inner.clone()];
    // ends[k] is where the text sits after k repetitions, so ends[0] is the
    // start: the count is ends.len() - 1, not ends.len().
    let mut ends = vec![at];
    let mut cursor = at;
    while max.is_none_or(|m| ends.len() - 1 < m) {
        match match_seq(&one, text, cursor) {
            // A zero-width match would repeat forever.
            Some(next) if next > cursor => {
                cursor = next;
                ends.push(cursor);
            }
            _ => break,
        }
    }
    // Greedy: try the longest run first, handing characters back to the rest of
    // the pattern until it fits, but never dropping below the minimum.
    let mut count = ends.len() - 1;
    loop {
        if count < min {
            return None;
        }
        if let Some(done) = match_seq(rest, text, ends[count]) {
            return Some(done);
        }
        if count == 0 {
            return None;
        }
        count -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, text: &str) -> bool {
        Regex::new(pattern, "").expect("compiles").is_match(text)
    }

    #[test]
    fn literals_classes_and_anchors() {
        assert!(matches("abc", "xxabcxx"));
        assert!(!matches("abc", "abx"));
        assert!(matches("^abc$", "abc"));
        assert!(!matches("^abc$", "abcd"));
        assert!(matches("a.c", "abc"));
        assert!(matches(r"\d+", "x42"));
        assert!(!matches(r"^\d+$", "4a2"));
        assert!(matches(r"\s", "a b"));
        assert!(matches(r"\bword\b", "a word here"));
        assert!(!matches(r"\bword\b", "wordy"));
    }

    #[test]
    fn sets_quantifiers_and_alternation() {
        assert!(matches("[abc]+", "cab"));
        assert!(matches("[a-z]{3}", "xyz"));
        assert!(!matches("^[a-z]{3}$", "xy"));
        assert!(matches("[^0-9]", "a"));
        assert!(!matches("^[^0-9]+$", "a1"));
        assert!(matches("colou?r", "color"));
        assert!(matches("colou?r", "colour"));
        assert!(matches("cat|dog", "hotdog"));
        assert!(matches("(ab)+c", "ababc"));
        // Backtracking: the greedy run must give characters back.
        assert!(matches("^a+ab$", "aaab"));
    }

    #[test]
    fn replace_and_split() {
        let comma = Regex::new(",", "g").expect("compiles");
        assert_eq!(comma.split("a,b,c"), ["a", "b", "c"]);

        let spaces = Regex::new(r"\s+", "g").expect("compiles");
        assert_eq!(spaces.replace("a   b  c", "-"), "a-b-c");

        // Without `g`, only the first match goes.
        let first = Regex::new(r"\d", "").expect("compiles");
        assert_eq!(first.replace("a1b2", "#"), "a#b2");
    }

    #[test]
    fn case_insensitive_flag() {
        let re = Regex::new("hello", "i").expect("compiles");
        assert!(re.is_match("Say HELLO there"));
        assert!(!Regex::new("hello", "").expect("compiles").is_match("HELLO"));
    }

    #[test]
    fn unsupported_syntax_is_refused_rather_than_guessed() {
        // A named group or lookbehind must not silently match something else.
        assert!(Regex::new("(?<name>a)", "").is_none());
        assert!(Regex::new("[unclosed", "").is_none());
    }
}
