//! A from-scratch JavaScript engine: lexer -> parser -> tree-walking interpreter.
//!
//! Phase 1 of the plan in docs/01-ARCHITECTURE.md §4: correctness over speed.
//! A bytecode VM with inline caches (Phase 2) and a baseline JIT (Phase 3) come later.

pub mod interp;
pub mod lexer;
pub mod parser;

pub use interp::Output;

/// Run a script, returning its console output, any `document.write` HTML, and errors.
/// Never panics: malformed input is reported as an error, not a crash.
pub fn run(source: &str) -> Output {
    let tokens = match lexer::tokenize(source) {
        Ok(t) => t,
        Err(e) => return err_output(format!("SyntaxError: {e}")),
    };
    let program = match parser::parse(tokens) {
        Ok(p) => p,
        Err(e) => return err_output(format!("SyntaxError: {e}")),
    };
    let mut interp = interp::Interp::new();
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
    fn document_write_is_captured() {
        let out = run("document.write('<p>hi</p>');");
        assert_eq!(out.writes, "<p>hi</p>");
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        assert!(!run("var x = ;").errors.is_empty()); // syntax error
        assert!(!run("nope();").errors.is_empty()); // runtime error
        assert!(!run("var x = 1 @ 2;").errors.is_empty()); // bad character
    }
}
