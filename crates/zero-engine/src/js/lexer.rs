//! JavaScript tokenizer.
//!
//! ponytail: no regex literals, template literals, or ASI subtleties — semicolons
//! and newlines are both treated as statement separators by the parser.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Kw(Kw),
    Op(String),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kw {
    Var,
    Function,
    Return,
    If,
    Else,
    While,
    For,
    This,
    New,
    Class,
    Extends,
    Super,
    Try,
    Catch,
    Finally,
    Throw,
    True,
    False,
    Null,
    Undefined,
}

fn keyword(word: &str) -> Option<Kw> {
    Some(match word {
        // `let`/`const` behave like `var` here — no block-scoping yet.
        "var" | "let" | "const" => Kw::Var,
        "function" => Kw::Function,
        "return" => Kw::Return,
        "if" => Kw::If,
        "else" => Kw::Else,
        "while" => Kw::While,
        "for" => Kw::For,
        "this" => Kw::This,
        "new" => Kw::New,
        "class" => Kw::Class,
        "extends" => Kw::Extends,
        "super" => Kw::Super,
        "try" => Kw::Try,
        "catch" => Kw::Catch,
        "finally" => Kw::Finally,
        "throw" => Kw::Throw,
        "true" => Kw::True,
        "false" => Kw::False,
        "null" => Kw::Null,
        "undefined" => Kw::Undefined,
        _ => return None,
    })
}

/// Longest-first so `===` wins over `==` over `=`.
const OPERATORS: &[&str] = &[
    "===", "!==", "==", "!=", "<=", ">=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "+", "-",
    "*", "/", "%", "=", "<", ">", "!", "(", ")", "{", "}", "[", "]", ",", ";", ".", ":", "?",
];

pub fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Comments
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        // Numbers
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let n = text
                .parse::<f64>()
                .map_err(|_| format!("bad number: {text}"))?;
            out.push(Tok::Num(n));
            continue;
        }
        // Strings
        if c == '"' || c == '\'' {
            i += 1;
            let mut s = String::new();
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                    s.push(match chars[i] {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                } else {
                    s.push(chars[i]);
                }
                i += 1;
            }
            if i >= chars.len() {
                return Err("unterminated string".to_string());
            }
            i += 1; // closing quote
            out.push(Tok::Str(s));
            continue;
        }
        // Identifiers / keywords
        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            out.push(match keyword(&word) {
                Some(k) => Tok::Kw(k),
                None => Tok::Ident(word),
            });
            continue;
        }
        // Operators
        let rest: String = chars[i..].iter().take(3).collect();
        match OPERATORS.iter().find(|op| rest.starts_with(**op)) {
            Some(op) => {
                out.push(Tok::Op((*op).to_string()));
                i += op.chars().count();
            }
            None => return Err(format!("unexpected character {c:?}")),
        }
    }

    out.push(Tok::Eof);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_basics() {
        let toks = tokenize("var x = 1 + 2; // note\n\"hi\"").unwrap();
        assert_eq!(toks[0], Tok::Kw(Kw::Var));
        assert_eq!(toks[1], Tok::Ident("x".into()));
        assert_eq!(toks[2], Tok::Op("=".into()));
        assert_eq!(toks[3], Tok::Num(1.0));
        assert_eq!(toks[6], Tok::Op(";".into()));
        assert_eq!(toks[7], Tok::Str("hi".into()));
    }

    #[test]
    fn prefers_longest_operator() {
        let toks = tokenize("a === b").unwrap();
        assert_eq!(toks[1], Tok::Op("===".into()));
    }
}
