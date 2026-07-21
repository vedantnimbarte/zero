//! A from-scratch JavaScript engine: lexer -> parser -> tree-walking interpreter.
//!
//! Phase 1 of the plan in docs/01-ARCHITECTURE.md §4: correctness over speed.
//! A bytecode VM with inline caches (Phase 2) and a baseline JIT (Phase 3) come later.

pub mod dom;
pub mod interp;
pub mod lexer;
pub mod parser;

pub use dom::{DomView, ElementInfo, Mutation};
pub use interp::Output;

/// Run a script, returning its console output, any `document.write` HTML, and errors.
/// Never panics: malformed input is reported as an error, not a crash.
pub fn run(source: &str) -> Output {
    run_with_dom(source, DomView::default())
}

/// Run a script against a document snapshot, so it can query and mutate elements.
pub fn run_with_dom(source: &str, dom: DomView) -> Output {
    let tokens = match lexer::tokenize(source) {
        Ok(t) => t,
        Err(e) => return err_output(format!("SyntaxError: {e}")),
    };
    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => return err_output(format!("SyntaxError: {e}")),
    };
    let mut interp = interp::Interp::with_dom(dom);
    interp.run(&program);
    interp.out
}

fn err_output(message: String) -> Output {
    Output { errors: vec![message], ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_variables_and_strings() {
        let out = run("var x = 2 + 3 * 4; console.log('x=' + x);");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["x=14"]); // precedence respected
    }

    #[test]
    fn functions_loops_and_conditionals() {
        let out = run(
            "function fact(n){ if (n <= 1) { return 1; } return n * fact(n - 1); }
             var total = 0;
             for (var i = 1; i <= 5; i++) { total += i; }
             console.log(fact(5));
             console.log(total);",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["120", "15"]);
    }

    #[test]
    fn closures_capture_their_defining_scope() {
        let out = run(
            "function counter() {
                 var n = 0;
                 return function() { n = n + 1; return n; };
             }
             var next = counter();
             next(); next();
             console.log(next());
             var other = counter();
             console.log(other());",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        // Each counter keeps its own captured `n`.
        assert_eq!(out.console, vec!["3", "1"]);
    }

    #[test]
    fn objects_and_arrays() {
        let out = run(
            "var user = { name: 'Zero', tags: ['fast', 'private'] };
             user.tags.push('indian');
             user.year = 2026;
             var nums = [1, 2, 3];
             nums[1] = 20;
             console.log(user.name + ' ' + user.year);
             console.log(user.tags.join('/'));
             console.log(nums[1] + nums.length);",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["Zero 2026", "fast/private/indian", "23"]);
    }

    #[test]
    fn functions_are_values() {
        let out = run(
            "function apply(f, v) { return f(v); }
             var double = function(x) { return x * 2; };
             console.log(apply(double, 21));",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["42"]);
    }

    #[test]
    fn document_write_is_captured() {
        let out = run("document.write('<p>hi</p>');");
        assert_eq!(out.writes, "<p>hi</p>");
    }

    #[test]
    fn reads_and_mutates_the_dom() {
        let dom = DomView {
            elements: vec![ElementInfo {
                path: vec![0],
                id: "out".into(),
                tag: "div".into(),
                text: "before".into(),
            }],
        };
        let out = run_with_dom(
            "var el = document.getElementById('out');
             console.log(el.textContent);
             el.textContent = 'after';",
            dom,
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["before"]); // read the snapshot
        assert!(matches!(out.mutations[..], [Mutation::SetText(0, ref s)] if s == "after"));
    }

    #[test]
    fn missing_element_is_null_not_an_error() {
        let out = run("var el = document.getElementById('nope'); console.log(el);");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["null"]);
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        assert!(!run("var x = ;").errors.is_empty()); // syntax error
        assert!(!run("nope();").errors.is_empty()); // runtime error
        assert!(!run("var x = 1 @ 2;").errors.is_empty()); // bad character
    }
}
