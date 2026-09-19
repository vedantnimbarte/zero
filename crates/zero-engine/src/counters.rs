//! CSS counters: the numbers behind numbered headings, figure captions,
//! footnotes and step-by-step instructions.
//!
//! Counters live on the style walk rather than on the element, because their
//! scoping is a property of document order and nothing else. A counter is
//! created by the element that resets it, and is visible to that element's
//! descendants *and its following siblings* — which is the whole reason
//! `h1 { counter-reset: section }` next to `h2 { counter-increment: section }`
//! works at all. So the scope ends when the parent runs out of children, not
//! when the resetting element's own subtree does.
//!
//! Each name holds a *stack*, not a value: an inner reset shadows an outer
//! counter of the same name, and `counters()` joins the whole stack, which is
//! what produces `1.2.3`. Nesting is by *depth*, though — a reset on a
//! following sibling replaces the counter its predecessor made rather than
//! nesting inside it, or a numbered list would read `1`, `1.1`, `1.1.1` down
//! the page instead of restarting.

use std::collections::HashMap;

/// The counters in scope at one point in the style walk.
#[derive(Default)]
pub struct Counters {
    /// Each name's nesting stack, innermost last, with the tree depth that
    /// created each level.
    scopes: HashMap<String, Vec<(i32, usize)>>,
    /// Every push made, in order, so a scope can be unwound exactly.
    journal: Vec<String>,
}

impl Counters {
    /// Where the journal stands, to be handed back to [`Counters::rewind`].
    pub fn mark(&self) -> usize {
        self.journal.len()
    }

    /// Drop every counter created since `mark`.
    pub fn rewind(&mut self, mark: usize) {
        while self.journal.len() > mark {
            let name = self.journal.pop().expect("checked");
            if let Some(stack) = self.scopes.get_mut(&name) {
                stack.pop();
            }
        }
    }

    /// `counter-reset: name n` — a new counter, shadowing any *outer* one.
    ///
    /// A reset at the depth that already holds this counter replaces it instead
    /// of nesting: that is a following sibling starting the count over, which is
    /// what `h2 { counter-reset: part }` means at every `h2` on the page.
    pub fn reset(&mut self, name: &str, value: i32, depth: usize) {
        let stack = self.scopes.entry(name.to_string()).or_default();
        match stack.last_mut() {
            Some(top) if top.1 == depth => top.0 = value,
            _ => {
                stack.push((value, depth));
                self.journal.push(name.to_string());
            }
        }
    }

    /// `counter-increment: name n`, on the innermost counter of that name.
    ///
    /// Incrementing a counter nothing reset creates it on the root, which is
    /// what CSS says to do rather than ignoring the declaration — pages rely on
    /// it, and a list that numbers itself without a `counter-reset` anywhere is
    /// common.
    pub fn increment(&mut self, name: &str, by: i32, depth: usize) {
        match self.scopes.get_mut(name).and_then(|stack| stack.last_mut()) {
            Some(top) => top.0 += by,
            None => self.reset(name, by, depth),
        }
    }

    /// `counter-set: name n`, which sets without creating a new scope.
    pub fn set(&mut self, name: &str, value: i32, depth: usize) {
        match self.scopes.get_mut(name).and_then(|stack| stack.last_mut()) {
            Some(top) => top.0 = value,
            None => self.reset(name, value, depth),
        }
    }

    /// `counter(name, style)` — the innermost value.
    pub fn value(&self, name: &str, style: &str) -> String {
        let value = self
            .scopes
            .get(name)
            .and_then(|stack| stack.last())
            .map(|top| top.0)
            .unwrap_or(0);
        format_counter(value, style)
    }

    /// `counters(name, separator, style)` — the whole nesting stack joined,
    /// which is how `1.2.3` is written.
    pub fn nested(&self, name: &str, separator: &str, style: &str) -> String {
        match self.scopes.get(name) {
            Some(stack) => stack
                .iter()
                .map(|(value, _)| format_counter(*value, style))
                .collect::<Vec<String>>()
                .join(separator),
            None => String::new(),
        }
    }

    /// Apply this element's `counter-reset` / `counter-increment` /
    /// `counter-set` declarations, in the order CSS applies them.
    pub fn apply(&mut self, values: &crate::style::PropertyMap, depth: usize) {
        // Reset before increment: `counter-reset: n; counter-increment: n` on
        // one element means "start over, then count one".
        for (property, default) in [("counter-reset", 0), ("counter-increment", 1)] {
            for (name, value) in parse_counter_list(values, property, default) {
                match property {
                    "counter-reset" => self.reset(&name, value, depth),
                    _ => self.increment(&name, value, depth),
                }
            }
        }
        for (name, value) in parse_counter_list(values, "counter-set", 0) {
            self.set(&name, value, depth);
        }
    }
}

/// `counter-reset: chapter 1 section` — pairs of a name and an optional number,
/// where a missing number is the property's own default.
fn parse_counter_list(
    values: &crate::style::PropertyMap,
    property: &str,
    default: i32,
) -> Vec<(String, i32)> {
    let text = match values.get(property) {
        Some(crate::css::Value::Raw(raw)) => raw.clone(),
        Some(crate::css::Value::Keyword(word)) => word.clone(),
        _ => return Vec::new(),
    };
    if text.trim().eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let mut out: Vec<(String, i32)> = Vec::new();
    for token in text.split_whitespace() {
        match token.parse::<i32>() {
            // A number belongs to the name before it.
            Ok(value) => {
                if let Some(last) = out.last_mut() {
                    last.1 = value;
                }
            }
            Err(_) => out.push((token.to_string(), default)),
        }
    }
    out
}

/// Turn a counter value into a marker.
///
/// The one place the engine formats an ordinal, so `list-style-type` shares it
/// rather than growing a second copy — the types are the same list.
pub fn format_counter(value: i32, style: &str) -> String {
    match style.trim() {
        "none" => String::new(),
        "decimal-leading-zero" => match value.abs() < 10 {
            true => format!("{}{:02}", if value < 0 { "-" } else { "" }, value.abs()),
            false => value.to_string(),
        },
        "lower-roman" => roman(value).to_lowercase(),
        "upper-roman" => roman(value),
        "lower-alpha" | "lower-latin" => alpha(value).to_lowercase(),
        "upper-alpha" | "upper-latin" => alpha(value),
        // `decimal` and anything this engine has no numbering system for. A
        // wrong number would be worse than a plain one.
        _ => value.to_string(),
    }
}

/// Roman numerals, which only exist for 1..=3999.
fn roman(value: i32) -> String {
    const NUMERALS: [(i32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    if !(1..=3999).contains(&value) {
        return value.to_string(); // outside the system; a number beats nonsense
    }
    let mut left = value;
    let mut out = String::new();
    for (amount, numeral) in NUMERALS {
        while left >= amount {
            out.push_str(numeral);
            left -= amount;
        }
    }
    out
}

/// `a`, `b`, ... `z`, `aa`, `ab` — bijective base 26, which is not quite the
/// same as base 26 (there is no zero digit).
fn alpha(value: i32) -> String {
    if value < 1 {
        return value.to_string();
    }
    let mut left = value;
    let mut out = Vec::new();
    while left > 0 {
        let digit = (left - 1) % 26;
        out.push((b'A' + digit as u8) as char);
        left = (left - 1) / 26;
    }
    out.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::Value;
    use crate::style::PropertyMap;

    fn declared(pairs: &[(&str, &str)]) -> PropertyMap {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), Value::Raw(value.to_string())))
            .collect()
    }

    #[test]
    fn counters_reset_increment_and_shadow() {
        let mut counters = Counters::default();
        counters.apply(&declared(&[("counter-reset", "section")]), 0);
        assert_eq!(counters.value("section", "decimal"), "0");
        counters.apply(&declared(&[("counter-increment", "section")]), 1);
        counters.apply(&declared(&[("counter-increment", "section")]), 1);
        assert_eq!(counters.value("section", "decimal"), "2");

        // An inner reset shadows the outer counter of the same name, and
        // `counters()` joins the whole stack — this is where `1.2.3` comes from.
        let mark = counters.mark();
        // A deeper reset nests rather than replacing.
        counters.apply(&declared(&[("counter-reset", "section")]), 2);
        counters.apply(&declared(&[("counter-increment", "section 3")]), 3);
        assert_eq!(counters.value("section", "decimal"), "3");
        assert_eq!(counters.nested("section", ".", "decimal"), "2.3");
        // Leaving the scope takes the inner counter with it.
        counters.rewind(mark);
        assert_eq!(counters.value("section", "decimal"), "2");
        assert_eq!(counters.nested("section", ".", "decimal"), "2");

        // Incrementing a counter nothing reset creates it rather than being
        // dropped: plenty of pages number a list without resetting anything.
        let mut bare = Counters::default();
        bare.apply(&declared(&[("counter-increment", "item")]), 1);
        assert_eq!(bare.value("item", "decimal"), "1");

        // An explicit step, several counters in one declaration, and `none`.
        let mut several = Counters::default();
        several.apply(&declared(&[("counter-reset", "a 5 b")]), 1);
        assert_eq!(several.value("a", "decimal"), "5");
        assert_eq!(several.value("b", "decimal"), "0");
        several.apply(&declared(&[("counter-increment", "none")]), 1);
        assert_eq!(several.value("a", "decimal"), "5");
        // `counter-set` writes the current counter instead of making a new one.
        several.apply(&declared(&[("counter-set", "a 9")]), 1);
        assert_eq!(several.value("a", "decimal"), "9");
        assert_eq!(several.nested("a", ".", "decimal"), "9");

        // A reset at the depth that already holds the counter starts it over
        // rather than nesting: two sibling sections each numbering their own
        // parts must both read `1`, not `1` then `1.1`.
        let mut siblings = Counters::default();
        siblings.apply(&declared(&[("counter-reset", "part")]), 1);
        siblings.apply(&declared(&[("counter-increment", "part")]), 2);
        assert_eq!(siblings.nested("part", ".", "decimal"), "1");
        siblings.apply(&declared(&[("counter-reset", "part")]), 1);
        siblings.apply(&declared(&[("counter-increment", "part")]), 2);
        assert_eq!(siblings.nested("part", ".", "decimal"), "1");
    }

    #[test]
    fn every_numbering_system_formats() {
        assert_eq!(format_counter(7, "decimal"), "7");
        assert_eq!(format_counter(7, "decimal-leading-zero"), "07");
        assert_eq!(format_counter(12, "decimal-leading-zero"), "12");
        assert_eq!(format_counter(4, "upper-roman"), "IV");
        assert_eq!(format_counter(1994, "lower-roman"), "mcmxciv");
        assert_eq!(format_counter(1, "lower-alpha"), "a");
        assert_eq!(format_counter(26, "upper-alpha"), "Z");
        // Bijective, so 27 is `aa` rather than `ba` or `a`.
        assert_eq!(format_counter(27, "lower-latin"), "aa");
        assert_eq!(format_counter(3, "none"), "");
        // Outside a system's range, a plain number beats nonsense.
        assert_eq!(format_counter(0, "upper-roman"), "0");
        assert_eq!(format_counter(0, "lower-alpha"), "0");
        assert_eq!(format_counter(5000, "upper-roman"), "5000");
        // An unknown type is decimal, not empty.
        assert_eq!(format_counter(5, "cjk-ideographic"), "5");
    }
}
