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
    fn classes_bind_this_and_construct_instances() {
        let out = run(
            "class Counter {
                 constructor(start) { this.n = start; }
                 bump(by) { this.n = this.n + by; return this.n; }
             }
             var a = new Counter(10);
             var b = new Counter(100);
             a.bump(5);
             console.log(a.bump(1));
             console.log(b.bump(1));",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        // Instances keep separate state, and `this` resolves inside methods.
        assert_eq!(out.console, vec!["16", "101"]);
    }

    #[test]
    fn classes_inherit_methods_and_call_super() {
        let out = run(
            "class Animal {
                 constructor(name) { this.name = name; }
                 speak() { return this.name + ' makes a sound'; }
                 describe() { return 'I am ' + this.name; }
             }
             class Dog extends Animal {
                 constructor(name) { super(name); this.legs = 4; }
                 speak() { return super.speak() + ' (a bark)'; }
             }
             var d = new Dog('Rex');
             console.log(d.speak());
             console.log(d.describe());
             console.log(d.legs);
             var a = new Animal('Cat');
             console.log(a.speak());",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(
            out.console,
            vec![
                "Rex makes a sound (a bark)", // override calling super
                "I am Rex",                   // inherited method
                "4",                          // subclass constructor ran after super()
                "Cat makes a sound",          // parent unaffected by the subclass
            ]
        );
    }

    #[test]
    fn this_works_on_object_methods() {
        let out = run(
            "var obj = { name: 'Zero', greet: function() { return 'hi ' + this.name; } };
             console.log(obj.greet());",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["hi Zero"]);
    }

    #[test]
    fn try_catch_recovers_and_finally_always_runs() {
        let out = run(
            "try { throw 'boom'; console.log('unreachable'); }
             catch (e) { console.log('caught ' + e); }
             finally { console.log('cleanup'); }
             console.log('after');",
        );
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["caught boom", "cleanup", "after"]);
    }

    #[test]
    fn try_catch_also_recovers_from_runtime_errors() {
        let out = run("try { missing(); } catch (e) { console.log('recovered'); }");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.console, vec!["recovered"]);
    }

    #[test]
    fn set_timeout_queues_callbacks_in_delay_order() {
        let mut interp = interp::Interp::new();
        let program = parser::parse(
            lexer::tokenize(
                "setTimeout(function(){ console.log('late'); }, 50);
                 setTimeout(function(){ console.log('early'); }, 5);
                 console.log('sync');",
            )
            .unwrap(),
        )
        .unwrap();
        interp.run(&program);
        assert_eq!(interp.out.console, vec!["sync"]); // nothing fired yet
        assert!(interp.run_timers());
        assert_eq!(interp.out.console, vec!["sync", "early", "late"]);
        assert!(!interp.run_timers()); // queue drained
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
                node_id: 1,
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
    fn click_handlers_fire_and_keep_closure_state() {
        // A counter closure registered as a handler must survive between clicks.
        let mut doc = crate::Document::load(
            "<html><body><div id='b'>0</div><script>
                 var n = 0;
                 var b = document.getElementById('b');
                 b.onclick = function() { n = n + 1; b.textContent = 'clicks: ' + n; };
             </script></body></html>",
            "",
        );
        // The <div> is the second element (html, body, div, script) -> node_id 3.
        assert!(doc.click(3), "handler should have fired");
        assert!(doc.click(3));
        assert!(!doc.click(999), "unknown element has no handler");
        assert!(doc.text_of(3).contains("clicks: 2"), "got {:?}", doc.text_of(3));
    }

    #[test]
    fn text_fields_accept_typing_and_are_readable_from_script() {
        let mut doc = crate::Document::load(
            "<html><body><input id='q' value='ab'><div id='out'>-</div>\
             <script>\
               var q = document.getElementById('q');\
               q.onclick = function() { document.getElementById('out').textContent = 'saw:' + q.value; };\
             </script></body></html>",
            "",
        );
        // html, body, input(3), div(4), script(5)
        assert!(doc.focus(3), "input should be focusable");
        assert!(doc.insert_text("cd"));
        assert!(doc.backspace());
        assert_eq!(doc.field_value(3), Some("abc"));

        // A script reads the typed value, not the original attribute.
        doc.blur();
        assert!(doc.click(3));
        assert!(doc.text_of(4).contains("saw:abc"), "got {:?}", doc.text_of(4));
    }

    #[test]
    fn script_can_set_a_field_value() {
        let mut doc = crate::Document::load(
            "<html><body><input id='q' value='old'>\
             <script>document.getElementById('q').value = 'new';</script></body></html>",
            "",
        );
        assert_eq!(doc.field_value(3), Some("new"));
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
