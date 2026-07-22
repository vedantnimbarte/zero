//! Tree-walking evaluator.
//!
//! Scopes form an environment chain, so a function captures the scope it was
//! defined in — that is what makes closures work.
//!
//! Still an AST interpreter rather than a bytecode VM, because measuring said
//! the AST walk was never the cost: cloning a loop body every time round the
//! loop was, then allocating a scope per iteration, then hashing variable names
//! with SipHash. Fixing those three made this 4–18x faster (see
//! `examples/jsbench.rs`) and left the evaluator small enough to read.
//!
//! ponytail: no inline caches or JIT (Phase 2/3 in docs/01-ARCHITECTURE.md §4).
//! A bytecode VM buys resolved variable slots, which is the next real win —
//! worth doing when a page's scripts, rather than a microbenchmark, are what is
//! slow. No prototypes either: objects are plain maps with a few built-ins.

use super::dom::{DomView, Mutation};
use super::parser::{Expr, Stmt};
use crate::resource::{KeyValueStore, ResourceLoader};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

type EnvRef = Rc<RefCell<Env>>;

/// Marks an error as a JS `throw` (catchable) rather than an engine failure.
const THROW_TAG: &str = "\u{1}throw\u{1}";

/// Hidden slot holding a class's parent method table, for `super`.
const SUPER_KEY: &str = "\u{1}super";

/// A hash built for short identifiers rather than for hostile keys.
///
/// The standard hasher is SipHash, which exists to make collisions hard to
/// arrange — the right default for a map holding data a site sent, and the
/// wrong one for a scope's variable names, which are looked up on every read of
/// every variable and dominated the cost of running a loop.
///
/// A page writes its own identifiers, so it could fill one scope with names
/// that collide here. It would be slowing down only its own variable lookups,
/// in a map that holds a handful of entries. Object properties — which *can*
/// hold keys straight out of `JSON.parse` — keep the standard hasher.
#[derive(Default, Clone, Copy)]
pub struct NameHasher(u64);

impl std::hash::Hasher for NameHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // FNV-1a: one multiply and one xor per byte, and identifiers are short.
        for byte in bytes {
            self.0 ^= *byte as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl std::hash::BuildHasher for NameHasher {
    type Hasher = NameHasher;

    fn build_hasher(&self) -> NameHasher {
        NameHasher(0xcbf2_9ce4_8422_2325) // the FNV offset basis
    }
}

/// A map keyed by names a script wrote, as opposed to data a site sent.
pub type NameMap<V> = HashMap<String, V, NameHasher>;

/// One lexical scope, linked to the scope enclosing it.
pub struct Env {
    vars: NameMap<Value>,
    parent: Option<EnvRef>,
}

impl Env {
    fn root() -> EnvRef {
        Rc::new(RefCell::new(Env {
            vars: NameMap::default(),
            parent: None,
        }))
    }

    fn child(parent: &EnvRef) -> EnvRef {
        Rc::new(RefCell::new(Env {
            vars: NameMap::default(),
            parent: Some(parent.clone()),
        }))
    }

    fn get(env: &EnvRef, name: &str) -> Option<Value> {
        let e = env.borrow();
        match e.vars.get(name) {
            Some(v) => Some(v.clone()),
            None => e.parent.as_ref().and_then(|p| Env::get(p, name)),
        }
    }

    /// Assign to an existing binding somewhere up the chain; returns false if unbound.
    fn set(env: &EnvRef, name: &str, value: Value) -> bool {
        let mut e = env.borrow_mut();
        // Overwrite in place: re-inserting would allocate a second copy of the
        // name on every assignment, which in a loop is most of the work.
        if let Some(slot) = e.vars.get_mut(name) {
            *slot = value;
            return true;
        }
        match e.parent.clone() {
            Some(parent) => {
                drop(e); // release before recursing
                Env::set(&parent, name, value)
            }
            None => false,
        }
    }

    fn define(env: &EnvRef, name: String, value: Value) {
        env.borrow_mut().vars.insert(name, value);
    }
}

/// Marks an object as a promise, holding the value it settled with.
///
/// A promise here is always *already* settled: `fetch` blocks, so there is
/// nothing to wait for. That makes `.then` a plain call and `await` an unwrap,
/// with no event loop and no microtask queue.
const PROMISE_KEY: &str = "__zero_settled";
/// Set on a promise that settled by rejecting.
const REJECTED_KEY: &str = "__zero_rejected";
/// The body of a `fetch` response, read by `.text()` and `.json()`.
const BODY_KEY: &str = "__zero_body";

/// Wrap a value in a settled promise.
fn promise(value: Value, rejected: bool) -> Value {
    let mut map = HashMap::new();
    map.insert(PROMISE_KEY.to_string(), value);
    if rejected {
        map.insert(REJECTED_KEY.to_string(), Value::Bool(true));
    }
    Value::Object(Rc::new(RefCell::new(map)))
}

/// What a value settles to: a promise's contents, or the value itself. A
/// rejected promise settles by throwing, exactly as `await` would.
fn settled(value: &Value) -> Result<Value, String> {
    match unwrap_promise(value) {
        Some((inner, true)) => Err(inner.to_display()),
        Some((inner, false)) => Ok(inner),
        None => Ok(value.clone()),
    }
}

/// `(settled value, was it a rejection)` if this is a promise.
fn unwrap_promise(value: &Value) -> Option<(Value, bool)> {
    let Value::Object(map) = value else { return None };
    let map = map.borrow();
    let inner = map.get(PROMISE_KEY)?.clone();
    Some((inner, map.contains_key(REJECTED_KEY)))
}

#[derive(Clone)]
pub enum Value {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    Func(Rc<FuncData>),
    /// A built-in implemented in Rust (console.log, document.write, ...).
    Native(&'static str),
    Object(Rc<RefCell<HashMap<String, Value>>>),
    Array(Rc<RefCell<Vec<Value>>>),
    /// A handle into the document snapshot (see [`super::dom`]).
    Element(usize),
    /// A compiled regular expression (see [`super::regex`]).
    Regex(Rc<super::regex::Regex>),
}

pub struct FuncData {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    /// The scope this function was created in — the essence of a closure.
    closure: EnvRef,
    /// Receiver bound at call time for `obj.method()` and class instances.
    this: Option<Box<Value>>,
}

impl Value {
    pub fn to_display(&self) -> String {
        match self {
            Value::Num(n) => {
                if n.fract() == 0.0 && n.is_finite() {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Value::Str(s) => s.clone(),
            Value::Regex(re) => format!("/{}/{}", re.source, re.flags),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".into(),
            Value::Undefined => "undefined".into(),
            Value::Func(_) | Value::Native(_) => "function".into(),
            Value::Array(items) => items
                .borrow()
                .iter()
                .map(Value::to_display)
                .collect::<Vec<_>>()
                .join(","),
            Value::Object(_) => "[object Object]".into(),
            Value::Element(_) => "[object HTMLElement]".into(),
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::Bool(b) => *b,
            Value::Null | Value::Undefined => false,
            _ => true,
        }
    }

    pub fn as_number(&self) -> f64 {
        match self {
            Value::Num(n) => *n,
            Value::Str(s) => s.trim().parse().unwrap_or(f64::NAN),
            Value::Bool(b) => *b as u8 as f64,
            Value::Null => 0.0,
            _ => f64::NAN,
        }
    }
}

/// What a statement did: fall through, or return from the enclosing function.
enum Flow {
    Normal,
    Return(Value),
}

/// A JavaScript exception travelling up the stack, distinct from an engine error.
pub struct Thrown(pub Value);

#[derive(Default)]
pub struct Output {
    pub console: Vec<String>,
    /// HTML emitted by `document.write`, appended to the document before layout.
    pub writes: String,
    pub errors: Vec<String>,
    /// DOM writes recorded by scripts, applied by the engine after the run.
    pub mutations: Vec<Mutation>,
    /// `input.value = x` writes, as (node id, text).
    pub field_writes: Vec<(usize, String)>,
}

pub struct Interp {
    env: EnvRef,
    depth: usize,
    dom: DomView,
    /// Event handlers keyed by (element node_id, event type), so they survive
    /// re-renders and one element can listen for several events.
    handlers: HashMap<(usize, String), Value>,
    /// Callbacks queued by setTimeout, ordered by delay then insertion.
    timers: Vec<(f64, usize, Value)>,
    timer_seq: usize,
    /// Supplied by the embedder so `fetch` can reach the network.
    loader: Option<Rc<dyn ResourceLoader>>,
    /// Backing store for `localStorage`, partitioned by the embedder.
    store: Option<Rc<dyn KeyValueStore>>,
    pub out: Output,
}

fn namespace(entries: &[(&str, &'static str)]) -> Value {
    let map: HashMap<String, Value> = entries
        .iter()
        .map(|(k, v)| (k.to_string(), Value::Native(v)))
        .collect();
    Value::Object(Rc::new(RefCell::new(map)))
}

impl Interp {
    pub fn new() -> Interp {
        Interp::with_dom(DomView::default())
    }

    pub fn with_dom(dom: DomView) -> Interp {
        let env = Env::root();
        Env::define(
            &env,
            "console".into(),
            namespace(&[("log", "console.log"), ("error", "console.log")]),
        );
        Env::define(&env, "setTimeout".into(), Value::Native("setTimeout"));
        Env::define(&env, "fetch".into(), Value::Native("fetch"));
        Env::define(
            &env,
            "JSON".into(),
            namespace(&[("parse", "JSON.parse"), ("stringify", "JSON.stringify")]),
        );
        Env::define(
            &env,
            "Promise".into(),
            namespace(&[
                ("resolve", "Promise.resolve"),
                ("reject", "Promise.reject"),
                ("all", "Promise.all"),
            ]),
        );
        Env::define(
            &env,
            "localStorage".into(),
            namespace(&[
                ("getItem", "localStorage.getItem"),
                ("setItem", "localStorage.setItem"),
                ("removeItem", "localStorage.removeItem"),
                ("clear", "localStorage.clear"),
            ]),
        );
        Env::define(
            &env,
            "document".into(),
            namespace(&[
                ("write", "document.write"),
                ("getElementById", "document.getElementById"),
                ("querySelector", "document.querySelector"),
                ("querySelectorAll", "document.querySelectorAll"),
                ("getElementsByClassName", "document.getElementsByClassName"),
                ("getElementsByTagName", "document.getElementsByTagName"),
            ]),
        );
        // `window` is the global object in a browser, and scripts reach for it
        // constantly — feature-detecting on it, or just calling
        // window.addEventListener. Without it they fail at the first mention.
        //
        // ponytail: the objects it holds are the same ones defined above; it is
        // not a live alias, so `window.foo = 1` does not create a global `foo`.
        let window = Value::Object(Rc::new(RefCell::new(HashMap::from([
            ("document".to_string(), Env::get(&env, "document").unwrap_or(Value::Undefined)),
            ("console".to_string(), Env::get(&env, "console").unwrap_or(Value::Undefined)),
            (
                "localStorage".to_string(),
                Env::get(&env, "localStorage").unwrap_or(Value::Undefined),
            ),
            ("setTimeout".to_string(), Value::Native("setTimeout")),
            ("fetch".to_string(), Value::Native("fetch")),
            // Listening is accepted and does nothing: the events these ask for
            // (load, resize, scroll) are not dispatched, and pretending to
            // register is better than failing the script outright.
            ("addEventListener".to_string(), Value::Native("window.addEventListener")),
            ("removeEventListener".to_string(), Value::Native("window.addEventListener")),
        ]))));
        Env::define(&env, "window".into(), window);

        Interp {
            env,
            depth: 0,
            dom,
            handlers: HashMap::new(),
            timers: Vec::new(),
            timer_seq: 0,
            loader: None,
            store: None,
            out: Output::default(),
        }
    }

    pub fn run(&mut self, program: &[Stmt]) {
        // Hoist function declarations so they can be called before their definition.
        for stmt in program {
            if let Stmt::FuncDecl { name, params, body } = stmt {
                let f = self.make_function(params.clone(), body.clone());
                Env::define(&self.env, name.clone(), f);
            }
        }
        for stmt in program {
            match self.exec(stmt) {
                Ok(Flow::Return(_)) => break,
                Ok(Flow::Normal) => {}
                Err(e) => {
                    self.out.errors.push(e);
                    break; // stop at the first error, like a thrown exception
                }
            }
        }
    }

    /// Give scripts network access through the embedder.
    pub fn set_loader(&mut self, loader: Rc<dyn ResourceLoader>) {
        self.loader = Some(loader);
    }

    /// Give scripts persistent key/value storage through the embedder.
    pub fn set_store(&mut self, store: Rc<dyn KeyValueStore>) {
        self.store = Some(store);
    }

    /// Refresh the snapshot after the document changed, so later events see new text.
    pub fn set_dom(&mut self, dom: DomView) {
        self.dom = dom;
    }

    pub fn has_handler(&self, node_id: usize, event: &str) -> bool {
        self.handlers.contains_key(&(node_id, event.to_string()))
    }

    /// Fire an element's handler for `event`. Returns false if it has none.
    pub fn dispatch(&mut self, node_id: usize, event: &str) -> bool {
        let handler = match self.handlers.get(&(node_id, event.to_string())) {
            Some(h) => h.clone(),
            None => return false,
        };
        if let Err(e) = self.call(handler, Vec::new()) {
            self.out.errors.push(e);
        }
        true
    }

    /// Mirror a write into the snapshot so the *same* script can read it back.
    ///
    /// The real DOM is still updated after the run; without this, a script that
    /// sets a class and then queries for it would find nothing.
    fn reflect(&mut self, index: usize, update: impl FnOnce(&mut super::dom::ElementInfo)) {
        if let Some(element) = self.dom.elements.get_mut(index) {
            update(element);
        }
    }

    fn node_id_of(&self, index: usize) -> Option<usize> {
        self.dom.elements.get(index).map(|e| e.node_id)
    }

    fn make_function(&self, params: Vec<String>, body: Vec<Stmt>) -> Value {
        Value::Func(Rc::new(FuncData {
            params,
            body,
            closure: self.env.clone(),
            this: None,
        }))
    }

    /// Same function, but called with `receiver` as `this`.
    fn bind_this(f: &Rc<FuncData>, receiver: Value) -> Value {
        Value::Func(Rc::new(FuncData {
            params: f.params.clone(),
            body: f.body.clone(),
            closure: f.closure.clone(),
            this: Some(Box::new(receiver)),
        }))
    }

    /// Run every queued timer callback, in delay order. Timers scheduled by a
    /// timer run on the next drain, so a self-rescheduling callback can't hang us.
    pub fn run_timers(&mut self) -> bool {
        if self.timers.is_empty() {
            return false;
        }
        let mut due = std::mem::take(&mut self.timers);
        due.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        for (_, _, callback) in due {
            if let Err(e) = self.call(callback, Vec::new()) {
                self.out.errors.push(e);
            }
        }
        true
    }

    /// Run `body` in a fresh child scope, restoring the previous scope afterwards.
    /// Enter a fresh scope, returning the one to restore afterwards.
    ///
    /// Deliberately not a closure taking `&mut self`: a closure would have to
    /// own the statements it runs, and *cloning a loop body every time round
    /// the loop* was costing more than executing it.
    fn push_scope(&mut self) -> EnvRef {
        let child = Env::child(&self.env);
        std::mem::replace(&mut self.env, child)
    }

    /// Does this block introduce a binding? Only then does it need a scope.
    fn declares(body: &[Stmt]) -> bool {
        body.iter().any(|stmt| {
            matches!(
                stmt,
                Stmt::VarDecl { .. } | Stmt::FuncDecl { .. } | Stmt::ClassDecl { .. }
            )
        })
    }

    fn run_for(
        &mut self,
        init: Option<&Stmt>,
        cond: Option<&Expr>,
        step: Option<&Expr>,
        body: &Stmt,
    ) -> Result<Flow, String> {
        if let Some(init) = init {
            self.exec(init)?;
        }
        let mut guard = 0;
        loop {
            let keep_going = match cond {
                Some(c) => self.eval(c)?.truthy(),
                None => true,
            };
            if !keep_going {
                return Ok(Flow::Normal);
            }
            if let Flow::Return(v) = self.exec(body)? {
                return Ok(Flow::Return(v));
            }
            if let Some(step) = step {
                self.eval(step)?;
            }
            guard += 1;
            // A page that loops forever must not take the browser with it.
            if guard > 1_000_000 {
                return Err("loop iteration limit exceeded".into());
            }
        }
    }

    fn exec(&mut self, stmt: &Stmt) -> Result<Flow, String> {
        match stmt {
            Stmt::VarDecl { names } => {
                for (name, init) in names {
                    let value = match init {
                        Some(e) => self.eval(e)?,
                        None => Value::Undefined,
                    };
                    Env::define(&self.env, name.clone(), value);
                }
                Ok(Flow::Normal)
            }
            Stmt::ExprStmt(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::Block(body) => {
                // A block that declares nothing cannot shadow anything, so it
                // needs no scope of its own — and a loop body runs this every
                // time round.
                if !Self::declares(body) {
                    return self.exec_body(body);
                }
                let saved = self.push_scope();
                let result = self.exec_body(body);
                self.env = saved;
                result
            }
            Stmt::If {
                cond,
                then,
                otherwise,
            } => {
                if self.eval(cond)?.truthy() {
                    self.exec(then)
                } else if let Some(alt) = otherwise {
                    self.exec(alt)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::While { cond, body } => {
                let mut guard = 0;
                while self.eval(cond)?.truthy() {
                    if let Flow::Return(v) = self.exec(body)? {
                        return Ok(Flow::Return(v));
                    }
                    guard += 1;
                    if guard > 1_000_000 {
                        return Err("loop iteration limit exceeded".into());
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::For {
                init,
                cond,
                step,
                body,
            } => {
                // The loop variable belongs to the loop, not to what surrounds it.
                let saved = self.push_scope();
                let result = self.run_for(init.as_deref(), cond.as_ref(), step.as_ref(), body);
                self.env = saved;
                result
            }
            Stmt::Return(value) => {
                let v = match value {
                    Some(e) => self.eval(e)?,
                    None => Value::Undefined,
                };
                Ok(Flow::Return(v))
            }
            Stmt::FuncDecl { name, params, body } => {
                let f = self.make_function(params.clone(), body.clone());
                Env::define(&self.env, name.clone(), f);
                Ok(Flow::Normal)
            }
            Stmt::Throw(expr) => {
                let v = self.eval(expr)?;
                // Exceptions ride the error channel, tagged so `catch` can recover.
                Err(format!("{THROW_TAG}{}", v.to_display()))
            }
            Stmt::Try {
                body,
                param,
                catch,
                finally,
            } => {
                let result = self.exec_body(body);
                let outcome = match result {
                    Err(e) => {
                        let message = e.strip_prefix(THROW_TAG).unwrap_or(&e).to_string();
                        let saved = self.env.clone();
                        self.env = Env::child(&saved);
                        if let Some(name) = param {
                            Env::define(&self.env, name.clone(), Value::Str(message));
                        }
                        let caught = self.exec_body(catch);
                        self.env = saved;
                        caught
                    }
                    ok => ok,
                };
                // `finally` runs regardless, and its own failure wins.
                if !finally.is_empty() {
                    self.exec_body(finally)?;
                }
                outcome
            }
            Stmt::ClassDecl {
                name,
                parent,
                methods,
            } => {
                // A class is an object of methods. `extends` copies the parent's
                // methods in first, so subclass definitions override them — a
                // flattened prototype chain rather than a linked one.
                let mut map = HashMap::new();
                let mut parent_table = None;
                if let Some(parent_name) = parent {
                    match Env::get(&self.env, parent_name) {
                        Some(Value::Object(base)) => {
                            map.extend(base.borrow().iter().map(|(k, v)| (k.clone(), v.clone())));
                            parent_table = Some(Value::Object(base.clone()));
                        }
                        _ => return Err(format!("{parent_name} is not a class")),
                    }
                }
                // Methods capture a scope where `super` is *this* class's parent.
                // Resolving it from the instance instead would make an inherited
                // constructor call itself, since the instance's parent is the
                // subclass's parent, not the defining class's.
                let saved = self.env.clone();
                self.env = Env::child(&saved);
                if let Some(parent) = parent_table {
                    Env::define(&self.env, SUPER_KEY.into(), parent);
                }
                for (method, params, body) in methods {
                    map.insert(
                        method.clone(),
                        self.make_function(params.clone(), body.clone()),
                    );
                }
                self.env = saved;
                Env::define(
                    &self.env,
                    name.clone(),
                    Value::Object(Rc::new(RefCell::new(map))),
                );
                Ok(Flow::Normal)
            }
        }
    }

    fn exec_body(&mut self, body: &[Stmt]) -> Result<Flow, String> {
        for stmt in body {
            if let Flow::Return(v) = self.exec(stmt)? {
                return Ok(Flow::Return(v));
            }
        }
        Ok(Flow::Normal)
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, String> {
        match expr {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Regex { pattern, flags } => match super::regex::Regex::new(pattern, flags) {
                Some(re) => Ok(Value::Regex(Rc::new(re))),
                // Refusing is better than matching the wrong thing silently.
                None => Err(format!("unsupported regular expression /{pattern}/{flags}")),
            },
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Ident(name) => {
                Env::get(&self.env, name).ok_or_else(|| format!("{name} is not defined"))
            }
            Expr::Func { params, body } => Ok(self.make_function(params.clone(), body.clone())),
            Expr::This => Ok(Env::get(&self.env, "this").unwrap_or(Value::Undefined)),
            Expr::Super => Ok(Env::get(&self.env, SUPER_KEY).unwrap_or(Value::Undefined)),
            Expr::Ternary {
                cond,
                then,
                otherwise,
            } => {
                // Only the taken branch is evaluated.
                if self.eval(cond)?.truthy() {
                    self.eval(then)
                } else {
                    self.eval(otherwise)
                }
            }
            Expr::New { callee, args } => {
                let class = self.eval(callee)?;
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a)?);
                }
                self.construct(class, values)
            }
            Expr::ArrayLit(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval(item)?);
                }
                Ok(Value::Array(Rc::new(RefCell::new(values))))
            }
            Expr::ObjectLit(props) => {
                let mut map = HashMap::new();
                for (key, expr) in props {
                    let v = self.eval(expr)?;
                    map.insert(key.clone(), v);
                }
                Ok(Value::Object(Rc::new(RefCell::new(map))))
            }
            // Nothing to wait for: `await` unwraps a settled promise, and a
            // rejected one throws exactly where the `await` stands.
            Expr::Unary { op, expr } if op == "await" => {
                let value = self.eval(expr)?;
                match unwrap_promise(&value) {
                    Some((inner, true)) => Err(format!("{THROW_TAG}{}", inner.to_display())),
                    Some((inner, false)) => Ok(inner),
                    None => Ok(value),
                }
            }
            Expr::Unary { op, expr } if op == "typeof" => {
                // `typeof` is how scripts ask whether something exists at all,
                // so an unknown name answers "undefined" instead of failing.
                let value = match self.eval(expr) {
                    Ok(value) => value,
                    Err(_) if matches!(**expr, Expr::Ident(_)) => Value::Undefined,
                    Err(e) => return Err(e),
                };
                Ok(Value::Str(type_name(&value).to_string()))
            }
            Expr::Unary { op, expr } => {
                let v = self.eval(expr)?;
                Ok(match op.as_str() {
                    "!" => Value::Bool(!v.truthy()),
                    "-" => Value::Num(-v.as_number()),
                    "~" => Value::Num(!to_i32(&v) as f64),
                    _ => Value::Num(v.as_number()),
                })
            }
            Expr::Sequence(parts) => {
                let mut last = Value::Undefined;
                for part in parts {
                    last = self.eval(part)?;
                }
                Ok(last)
            }
            Expr::Binary { op, left, right } => {
                // Short-circuit before evaluating the right side.
                if op == "&&" {
                    let l = self.eval(left)?;
                    return if l.truthy() { self.eval(right) } else { Ok(l) };
                }
                if op == "||" {
                    let l = self.eval(left)?;
                    return if l.truthy() { Ok(l) } else { self.eval(right) };
                }
                // `??` differs from `||`: it only falls through for null and
                // undefined, so an empty string or 0 on the left still wins.
                if op == "??" {
                    let l = self.eval(left)?;
                    return match l {
                        Value::Undefined | Value::Null => self.eval(right),
                        value => Ok(value),
                    };
                }
                let l = self.eval(left)?;
                let r = self.eval(right)?;
                Ok(binary_op(op, &l, &r))
            }
            Expr::Assign { target, value } => {
                let v = self.eval(value)?;
                self.assign_to(target, v.clone())?;
                Ok(v)
            }
            Expr::Member { object, property } => {
                let obj = self.eval(object)?;
                Ok(self.get_property(&obj, property))
            }
            Expr::Index { object, index } => {
                let obj = self.eval(object)?;
                let key = self.eval(index)?;
                Ok(match (&obj, &key) {
                    (Value::Array(items), _) => {
                        let i = key.as_number();
                        let items = items.borrow();
                        if i >= 0.0 && (i as usize) < items.len() {
                            items[i as usize].clone()
                        } else {
                            Value::Undefined
                        }
                    }
                    _ => self.get_property(&obj, &key.to_display()),
                })
            }
            Expr::Call { callee, args } => {
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a)?);
                }
                // `super(...)` calls the parent constructor on the current `this`.
                if matches!(**callee, Expr::Super) {
                    let parent = Env::get(&self.env, SUPER_KEY).unwrap_or(Value::Undefined);
                    let this = Env::get(&self.env, "this").unwrap_or(Value::Undefined);
                    let ctor = self.get_property(&parent, "constructor");
                    if let Value::Func(ref data) = ctor {
                        let bound = Interp::bind_this(data, this);
                        return self.call(bound, values);
                    }
                    return Ok(Value::Undefined);
                }
                // Method calls need the receiver, so handle `obj.m()` specially.
                if let Expr::Member { object, property } = &**callee {
                    let receiver = self.eval(object)?;
                    // `super.m()` runs the parent's method against the current `this`.
                    if matches!(**object, Expr::Super) {
                        let this = Env::get(&self.env, "this").unwrap_or(Value::Undefined);
                        let method = self.get_property(&receiver, property);
                        if let Value::Func(ref data) = method {
                            let bound = Interp::bind_this(data, this);
                            return self.call(bound, values);
                        }
                        return Ok(Value::Undefined);
                    }
                    if let Some(result) = self.call_method(&receiver, property, &values)? {
                        return Ok(result);
                    }
                    let f = self.get_property(&receiver, property);
                    // `obj.method()` binds the receiver so the body can use `this`.
                    let f = match f {
                        Value::Func(ref data) => Interp::bind_this(data, receiver),
                        other => other,
                    };
                    return self.call(f, values);
                }
                let f = self.eval(callee)?;
                self.call(f, values)
            }
        }
    }

    fn assign_to(&mut self, target: &Expr, v: Value) -> Result<(), String> {
        match target {
            Expr::Ident(name) => {
                if !Env::set(&self.env, name, v.clone()) {
                    Env::define(&self.env, name.clone(), v); // implicit global
                }
                Ok(())
            }
            Expr::Member { object, property } => {
                match self.eval(object)? {
                    Value::Object(map) => {
                        map.borrow_mut().insert(property.clone(), v);
                    }
                    Value::Element(i) => match property.as_str() {
                        // Field text lives in the document's form state, not the DOM.
                        "value" => {
                            if let Some(id) = self.node_id_of(i) {
                                self.out.field_writes.push((id, v.to_display()));
                            }
                            self.reflect(i, |e| e.text = v.to_display());
                        }
                        // `onclick`, `oninput`, `onchange`, ... all register the same way.
                        name if name.starts_with("on") => {
                            if let Some(id) = self.node_id_of(i) {
                                self.handlers.insert((id, name[2..].to_string()), v);
                            }
                        }
                        "textContent" | "innerText" => {
                            self.out
                                .mutations
                                .push(Mutation::SetText(i, v.to_display()));
                            self.reflect(i, |e| e.text = v.to_display());
                        }
                        "innerHTML" => {
                            self.out
                                .mutations
                                .push(Mutation::SetHtml(i, v.to_display()));
                            self.reflect(i, |e| e.text = v.to_display());
                        }
                        // Restyling: swapping the class re-runs the cascade for this node.
                        "className" => {
                            self.out
                                .mutations
                                .push(Mutation::SetClass(i, v.to_display()));
                            self.reflect(i, |e| e.class = v.to_display());
                        }
                        _ => {} // other properties aren't modelled yet
                    },
                    _ => {}
                }
                Ok(())
            }
            Expr::Index { object, index } => {
                let obj = self.eval(object)?;
                let key = self.eval(index)?;
                match obj {
                    Value::Array(items) => {
                        let i = key.as_number();
                        if i >= 0.0 {
                            let mut items = items.borrow_mut();
                            let i = i as usize;
                            if i >= items.len() {
                                items.resize(i + 1, Value::Undefined);
                            }
                            items[i] = v;
                        }
                    }
                    Value::Object(map) => {
                        map.borrow_mut().insert(key.to_display(), v);
                    }
                    _ => {}
                }
                Ok(())
            }
            _ => Err("invalid assignment target".into()),
        }
    }

    fn get_property(&self, obj: &Value, property: &str) -> Value {
        match obj {
            Value::Object(map) => map
                .borrow()
                .get(property)
                .cloned()
                .unwrap_or(Value::Undefined),
            Value::Array(items) if property == "length" => Value::Num(items.borrow().len() as f64),
            Value::Str(s) if property == "length" => Value::Num(s.chars().count() as f64),
            Value::Element(i) => self.element_property(*i, property),
            _ => Value::Undefined,
        }
    }

    /// Built-in methods on arrays and strings. `None` means "not a built-in".
    fn call_method(
        &mut self,
        receiver: &Value,
        method: &str,
        args: &[Value],
    ) -> Result<Option<Value>, String> {
        // Promise plumbing first: `.then` on a settled promise is just a call,
        // and a script may chain it on anything an async function returned.
        //
        // ponytail: `async` is parsed and ignored, so a user function's return
        // value is not auto-wrapped. Treating any receiver as already settled
        // keeps `f().then(...)` working; the cost is that `.then` on a plain
        // object calls back with the object instead of failing.
        if matches!(method, "then" | "catch" | "finally")
            && !matches!(receiver, Value::Object(map) if map.borrow().contains_key(method))
        {
            let (value, rejected) = unwrap_promise(receiver).unwrap_or((receiver.clone(), false));
            let handler = match method {
                "then" if !rejected => args.first(),
                "then" => args.get(1), // the second argument is the reject path
                "catch" if rejected => args.first(),
                "finally" => args.first(),
                _ => None,
            };
            let Some(handler) = handler.cloned() else {
                return Ok(Some(promise(value, rejected)));
            };
            let call_args = match method {
                "finally" => Vec::new(),
                _ => vec![value.clone()],
            };
            let produced = self.call(handler, call_args)?;
            // `finally` passes the original settlement through untouched.
            return Ok(Some(match method {
                "finally" => promise(value, rejected),
                // A handler that returns a promise flattens, as chaining requires.
                _ => match unwrap_promise(&produced) {
                    Some((inner, rejected)) => promise(inner, rejected),
                    None => promise(produced, false),
                },
            }));
        }

        let result = match (receiver, method) {
            // `fetch` responses. Both are settled promises, so `await res.json()`
            // and `res.json().then(...)` both work.
            (Value::Object(map), "text") if map.borrow().contains_key(BODY_KEY) => {
                promise(map.borrow()[BODY_KEY].clone(), false)
            }
            (Value::Object(map), "json") if map.borrow().contains_key(BODY_KEY) => {
                let body = map.borrow()[BODY_KEY].to_display();
                match parse_json(&body) {
                    Some(value) => promise(value, false),
                    None => promise(Value::Str("SyntaxError: bad JSON".into()), true),
                }
            }
            (Value::Array(items), "push") => {
                items.borrow_mut().extend(args.iter().cloned());
                Value::Num(items.borrow().len() as f64)
            }
            (Value::Array(items), "pop") => items.borrow_mut().pop().unwrap_or(Value::Undefined),
            (Value::Array(items), "join") => {
                let sep = args
                    .first()
                    .map(Value::to_display)
                    .unwrap_or_else(|| ",".into());
                let joined = items
                    .borrow()
                    .iter()
                    .map(Value::to_display)
                    .collect::<Vec<_>>()
                    .join(&sep);
                Value::Str(joined)
            }
            (Value::Element(i), "addEventListener") => {
                let event = args.first().map(Value::to_display);
                if let (Some(event), Some(f), Some(id)) = (event, args.get(1), self.node_id_of(*i))
                {
                    self.handlers.insert((id, event), f.clone());
                }
                Value::Undefined
            }
            // Regex methods, and the string methods that accept one.
            (Value::Regex(re), "test") => {
                Value::Bool(re.is_match(&args.first().map(Value::to_display).unwrap_or_default()))
            }
            (Value::Str(s), "replace" | "replaceAll") => match args.first() {
                Some(Value::Regex(re)) => {
                    let with = args.get(1).map(Value::to_display).unwrap_or_default();
                    Value::Str(re.replace(s, &with))
                }
                Some(needle) => {
                    let needle = needle.to_display();
                    let with = args.get(1).map(Value::to_display).unwrap_or_default();
                    Value::Str(match method {
                        "replaceAll" => s.replace(&needle, &with),
                        _ => s.replacen(&needle, &with, 1),
                    })
                }
                None => Value::Str(s.clone()),
            },
            (Value::Str(s), "split") => {
                let parts: Vec<Value> = match args.first() {
                    Some(Value::Regex(re)) => {
                        re.split(s).into_iter().map(Value::Str).collect()
                    }
                    Some(sep) => s
                        .split(&sep.to_display())
                        .map(|p| Value::Str(p.to_string()))
                        .collect(),
                    None => vec![Value::Str(s.clone())],
                };
                Value::Array(Rc::new(RefCell::new(parts)))
            }
            (Value::Str(s), "match") => match args.first() {
                Some(Value::Regex(re)) => match re.find(s) {
                    Some((start, end)) => {
                        let hit: String = s.chars().skip(start).take(end - start).collect();
                        Value::Array(Rc::new(RefCell::new(vec![Value::Str(hit)])))
                    }
                    None => Value::Null,
                },
                _ => Value::Null,
            },
            (Value::Str(s), "toUpperCase") => Value::Str(s.to_uppercase()),
            (Value::Str(s), "toLowerCase") => Value::Str(s.to_lowercase()),
            _ => return Ok(None),
        };
        Ok(Some(result))
    }

    fn element_property(&self, index: usize, property: &str) -> Value {
        let element = match self.dom.elements.get(index) {
            Some(e) => e,
            None => return Value::Undefined,
        };
        match property {
            // A field's rendered text is its value (minus the caret).
            "value" => Value::Str(element.text.trim_end_matches('|').to_string()),
            "textContent" | "innerText" | "innerHTML" => Value::Str(element.text.clone()),
            "id" => Value::Str(element.id.clone()),
            "className" => Value::Str(element.class.clone()),
            "tagName" => Value::Str(element.tag.to_ascii_uppercase()),
            _ => Value::Undefined,
        }
    }

    /// `new C(...)`: copy the class's methods onto a fresh object, bind `this`,
    /// then run `constructor` if present.
    fn construct(&mut self, class: Value, args: Vec<Value>) -> Result<Value, String> {
        let methods = match class {
            Value::Object(ref map) => map.borrow().clone(),
            other => return Err(format!("{} is not a constructor", other.to_display())),
        };
        let instance = Value::Object(Rc::new(RefCell::new(HashMap::new())));
        if let Value::Object(ref map) = instance {
            for (name, method) in &methods {
                let bound = match method {
                    Value::Func(data) => Interp::bind_this(data, instance.clone()),
                    other => other.clone(),
                };
                map.borrow_mut().insert(name.clone(), bound);
            }
        }
        if let Some(ctor) = methods.get("constructor") {
            if let Value::Func(data) = ctor {
                let bound = Interp::bind_this(data, instance.clone());
                self.call(bound, args)?;
            }
        }
        Ok(instance)
    }

    fn call(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, String> {
        match callee {
            Value::Native(name) => {
                let text = args
                    .iter()
                    .map(Value::to_display)
                    .collect::<Vec<_>>()
                    .join(" ");
                match name {
                    "console.log" => self.out.console.push(text),
                    "document.write" => self.out.writes.push_str(&text),
                    // Accepted and ignored: load/resize/scroll are never fired.
                    "window.addEventListener" => {}
                    "setTimeout" => {
                        // No real clock: callbacks queue and the embedder drains them.
                        let delay = args.get(1).map(Value::as_number).unwrap_or(0.0);
                        if let Some(callback) = args.first() {
                            self.timer_seq += 1;
                            let seq = self.timer_seq;
                            self.timers.push((delay, seq, callback.clone()));
                            return Ok(Value::Num(seq as f64));
                        }
                        return Ok(Value::Undefined);
                    }
                    "fetch" => {
                        // ponytail: the request happens here and now, and the
                        // Promise it returns is already settled. Scripts written
                        // against `.then`/`await` work; anything relying on the
                        // callback running *later* does not.
                        let body = self
                            .loader
                            .as_ref()
                            .and_then(|l| l.load(&text))
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
                        let mut response = HashMap::new();
                        response.insert("ok".into(), Value::Bool(body.is_some()));
                        response.insert(
                            "status".into(),
                            Value::Num(if body.is_some() { 200.0 } else { 0.0 }),
                        );
                        response.insert(BODY_KEY.into(), Value::Str(body.unwrap_or_default()));
                        return Ok(promise(Value::Object(Rc::new(RefCell::new(response))), false));
                    }
                    "Promise.resolve" => {
                        return Ok(promise(
                            args.first().cloned().unwrap_or(Value::Undefined),
                            false,
                        ))
                    }
                    "Promise.reject" => {
                        return Ok(promise(
                            args.first().cloned().unwrap_or(Value::Undefined),
                            true,
                        ))
                    }
                    // Every promise here is already settled, so "all of them" is
                    // just their values in order.
                    "Promise.all" => {
                        let items = match args.first() {
                            Some(Value::Array(items)) => items.borrow().clone(),
                            _ => Vec::new(),
                        };
                        let mut out = Vec::with_capacity(items.len());
                        for item in items {
                            out.push(settled(&item)?);
                        }
                        return Ok(promise(
                            Value::Array(Rc::new(RefCell::new(out))),
                            false,
                        ));
                    }
                    "JSON.parse" => {
                        let text = args.first().map(Value::to_display).unwrap_or_default();
                        return parse_json(&text)
                            .ok_or_else(|| "SyntaxError: bad JSON".to_string());
                    }
                    "JSON.stringify" => {
                        return Ok(Value::Str(
                            args.first().map(stringify_json).unwrap_or_default(),
                        ))
                    }
                    "localStorage.getItem" => {
                        let key = args.first().map(Value::to_display).unwrap_or_default();
                        return Ok(match self.store.as_ref().and_then(|s| s.get(&key)) {
                            Some(v) => Value::Str(v),
                            None => Value::Null, // absent keys read as null, like the web
                        });
                    }
                    "localStorage.setItem" => {
                        if let (Some(store), Some(key)) = (&self.store, args.first()) {
                            let value = args.get(1).map(Value::to_display).unwrap_or_default();
                            store.set(&key.to_display(), &value);
                        }
                        return Ok(Value::Undefined);
                    }
                    "localStorage.removeItem" => {
                        if let (Some(store), Some(key)) = (&self.store, args.first()) {
                            store.remove(&key.to_display());
                        }
                        return Ok(Value::Undefined);
                    }
                    "localStorage.clear" => {
                        if let Some(store) = &self.store {
                            store.clear();
                        }
                        return Ok(Value::Undefined);
                    }
                    "document.querySelector" => {
                        return Ok(match self.dom.query(&text).first() {
                            Some(i) => Value::Element(*i),
                            None => Value::Null,
                        })
                    }
                    "document.querySelectorAll"
                    | "document.getElementsByClassName"
                    | "document.getElementsByTagName" => {
                        // The two legacy helpers are just selectors in disguise.
                        let selector = match name {
                            "document.getElementsByClassName" => format!(".{text}"),
                            "document.getElementsByTagName" => text.clone(),
                            _ => text.clone(),
                        };
                        let found: Vec<Value> = self
                            .dom
                            .query(&selector)
                            .into_iter()
                            .map(Value::Element)
                            .collect();
                        return Ok(Value::Array(Rc::new(RefCell::new(found))));
                    }
                    "document.getElementById" => {
                        return Ok(match self.dom.find_by_id(&text) {
                            Some(i) => Value::Element(i),
                            None => Value::Null,
                        })
                    }
                    _ => return Err(format!("unknown builtin {name}")),
                }
                Ok(Value::Undefined)
            }
            Value::Func(f) => {
                if self.depth > 200 {
                    return Err("maximum call depth exceeded".into());
                }
                // Calls run in a child of the *defining* scope, not the calling one.
                let saved = self.env.clone();
                self.env = Env::child(&f.closure);
                if let Some(receiver) = &f.this {
                    Env::define(&self.env, "this".into(), (**receiver).clone());
                }
                for (i, param) in f.params.iter().enumerate() {
                    Env::define(
                        &self.env,
                        param.clone(),
                        args.get(i).cloned().unwrap_or(Value::Undefined),
                    );
                }
                self.depth += 1;
                let result = self.exec_body(&f.body);
                self.depth -= 1;
                self.env = saved;
                match result? {
                    Flow::Return(v) => Ok(v),
                    Flow::Normal => Ok(Value::Undefined),
                }
            }
            other => Err(format!("{} is not a function", other.to_display())),
        }
    }
}

fn binary_op(op: &str, l: &Value, r: &Value) -> Value {
    match op {
        // `+` concatenates if either side is a string, like JS.
        "+" => match (l, r) {
            (Value::Str(_), _) | (_, Value::Str(_)) => {
                Value::Str(format!("{}{}", l.to_display(), r.to_display()))
            }
            _ => Value::Num(l.as_number() + r.as_number()),
        },
        "-" => Value::Num(l.as_number() - r.as_number()),
        "*" => Value::Num(l.as_number() * r.as_number()),
        "/" => Value::Num(l.as_number() / r.as_number()),
        "%" => Value::Num(l.as_number() % r.as_number()),
        "<" => Value::Bool(l.as_number() < r.as_number()),
        ">" => Value::Bool(l.as_number() > r.as_number()),
        "<=" => Value::Bool(l.as_number() <= r.as_number()),
        ">=" => Value::Bool(l.as_number() >= r.as_number()),
        "==" | "===" => Value::Bool(loose_eq(l, r)),
        "!=" | "!==" => Value::Bool(!loose_eq(l, r)),
        "**" => Value::Num(l.as_number().powf(r.as_number())),
        // Bitwise work on 32-bit integers in JS, and shifts count modulo 32.
        "&" => Value::Num((to_i32(l) & to_i32(r)) as f64),
        "|" => Value::Num((to_i32(l) | to_i32(r)) as f64),
        "^" => Value::Num((to_i32(l) ^ to_i32(r)) as f64),
        "<<" => Value::Num((to_i32(l) << (to_u32(r) & 31)) as f64),
        ">>" => Value::Num((to_i32(l) >> (to_u32(r) & 31)) as f64),
        ">>>" => Value::Num(((to_i32(l) as u32) >> (to_u32(r) & 31)) as f64),
        _ => Value::Undefined,
    }
}

/// JS converts to a signed 32-bit integer for bitwise work, wrapping rather
/// than saturating — NaN and infinities become zero.
fn to_i32(value: &Value) -> i32 {
    let n = value.as_number();
    if !n.is_finite() {
        return 0;
    }
    (n.trunc() as i64 & 0xffff_ffff) as u32 as i32
}

fn to_u32(value: &Value) -> u32 {
    to_i32(value) as u32
}

/// What `typeof` reports. Arrays and elements are objects, as in a browser.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Num(_) => "number",
        Value::Str(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Undefined => "undefined",
        Value::Func(_) | Value::Native(_) => "function",
        Value::Null | Value::Object(_) | Value::Array(_) | Value::Element(_) | Value::Regex(_) => {
            "object"
        }
    }
}

fn loose_eq(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Null | Value::Undefined, Value::Null | Value::Undefined) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        _ => l.as_number() == r.as_number(),
    }
}

/// A JSON reader. Deliberately its own parser rather than the JS one: JSON is a
/// data format from untrusted servers, and it must not accept expressions.
fn parse_json(text: &str) -> Option<Value> {
    let mut chars: Vec<char> = text.chars().collect();
    chars.push('\0'); // sentinel, so peeking past the end is not a special case
    let mut pos = 0;
    let value = json_value(&chars, &mut pos)?;
    json_space(&chars, &mut pos);
    match chars[pos] {
        '\0' => Some(value),
        _ => None, // trailing junk: not one JSON document
    }
}

fn json_space(chars: &[char], pos: &mut usize) {
    while chars[*pos].is_whitespace() {
        *pos += 1;
    }
}

fn json_value(chars: &[char], pos: &mut usize) -> Option<Value> {
    json_space(chars, pos);
    match chars[*pos] {
        '"' => Some(Value::Str(json_string(chars, pos)?)),
        '[' => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                json_space(chars, pos);
                if chars[*pos] == ']' {
                    *pos += 1;
                    return Some(Value::Array(Rc::new(RefCell::new(items))));
                }
                items.push(json_value(chars, pos)?);
                json_space(chars, pos);
                match chars[*pos] {
                    ',' => *pos += 1,
                    ']' => {}
                    _ => return None,
                }
            }
        }
        '{' => {
            *pos += 1;
            let mut map = HashMap::new();
            loop {
                json_space(chars, pos);
                if chars[*pos] == '}' {
                    *pos += 1;
                    return Some(Value::Object(Rc::new(RefCell::new(map))));
                }
                let key = json_string(chars, pos)?;
                json_space(chars, pos);
                if chars[*pos] != ':' {
                    return None;
                }
                *pos += 1;
                map.insert(key, json_value(chars, pos)?);
                json_space(chars, pos);
                match chars[*pos] {
                    ',' => *pos += 1,
                    '}' => {}
                    _ => return None,
                }
            }
        }
        't' | 'f' | 'n' => {
            for (word, value) in [
                ("true", Value::Bool(true)),
                ("false", Value::Bool(false)),
                ("null", Value::Null),
            ] {
                if chars[*pos..].starts_with(&word.chars().collect::<Vec<char>>()[..]) {
                    *pos += word.len();
                    return Some(value);
                }
            }
            None
        }
        _ => {
            let start = *pos;
            while matches!(chars[*pos], '0'..='9' | '-' | '+' | '.' | 'e' | 'E') {
                *pos += 1;
            }
            chars[start..*pos]
                .iter()
                .collect::<String>()
                .parse()
                .ok()
                .map(Value::Num)
        }
    }
}

fn json_string(chars: &[char], pos: &mut usize) -> Option<String> {
    if chars[*pos] != '"' {
        return None;
    }
    *pos += 1;
    let mut out = String::new();
    loop {
        let c = chars[*pos];
        *pos += 1;
        match c {
            '"' => return Some(out),
            '\0' => return None, // unterminated
            '\\' => {
                let escape = chars[*pos];
                *pos += 1;
                out.push(match escape {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'u' => {
                        let hex: String = chars[*pos..*pos + 4].iter().collect();
                        *pos += 4;
                        char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
                    }
                    other => other, // `\"`, `\\`, `\/`
                });
            }
            other => out.push(other),
        }
    }
}

fn stringify_json(value: &Value) -> String {
    match value {
        Value::Num(n) => Value::Num(*n).to_display(),
        Value::Bool(b) => b.to_string(),
        Value::Null | Value::Undefined => "null".to_string(),
        Value::Str(s) => quote_json(s),
        Value::Array(items) => {
            let parts: Vec<String> = items.borrow().iter().map(stringify_json).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            // Sorted, because a HashMap has no order of its own and a stringify
            // that shuffled its keys between runs would be untestable.
            let map = map.borrow();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", quote_json(k), stringify_json(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        // Functions and elements have no JSON form; a browser drops them.
        _ => "null".to_string(),
    }
}

fn quote_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str(r"\n"),
            '\t' => out.push_str(r"\t"),
            '\r' => out.push_str(r"\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
