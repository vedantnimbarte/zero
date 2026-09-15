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
//!
//! Objects, arrays, functions and scopes all live in arenas owned by
//! [`Interp`] (`objects`/`arrays`/`funcs`/`envs`), addressed by [`Value`]
//! through a plain `u32` index rather than an `Rc`. A closure capturing the
//! scope it was defined in, which itself holds a variable pointing back at
//! that closure, used to be a real `Rc` cycle — leaked forever, once per
//! navigation. With everything owned by the arena instead, dropping the
//! `Interp` (a tab navigating, or its process exiting) reclaims all of it
//! regardless of which values pointed at which; nothing here needs to know a
//! cycle existed. A function's `params`/`body` stay `Rc`-shared *within* its
//! arena slot — that Rc can never be part of a cycle, since AST nodes hold no
//! `Value`, so sharing it costs nothing and keeps a hot call from re-cloning
//! a function's whole body on every invocation.
//!
//! ponytail: the arena only grows — no id is ever freed mid-run, so a script
//! that keeps creating closures (many `setInterval` callbacks) grows it
//! without bound. A mark-sweep pass over these same four `Vec`s is the
//! addable fix, shipped when a long-lived tab is *measured* growing.

use super::dom::{DomView, Mutation};
use super::parser::{DeclKind, Expr, Stmt};
use crate::resource::{KeyValueStore, ResourceLoader};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

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

/// One lexical scope, linked to the scope enclosing it by arena index.
struct EnvData {
    vars: NameMap<Value>,
    parent: Option<u32>,
    /// Whether `var` stops climbing here rather than continuing past it —
    /// set for a function call's own scope and the global root, so a `var`
    /// declared inside a nested `if`/`for`/block still lands in the function
    /// it's part of (real hoisting), while `let`/`const` land exactly where
    /// `Interp::env_define` is called, which is always the block that
    /// declared them.
    is_function_scope: bool,
}

/// A user function or method, addressed by arena index (`Value::Func`).
///
/// `params`/`body` are `Rc`-shared rather than cloned per call: a call can't
/// hold a borrow of `self.funcs` while it runs the body (the body itself
/// mutates `self`), so the alternative is a deep `Vec<Stmt>` clone on every
/// invocation — exactly the cost a previous pass on this file measured and
/// removed. Neither `Rc` can form a cycle: AST nodes hold no `Value`.
struct FuncData {
    params: Rc<Vec<String>>,
    body: Rc<Vec<Stmt>>,
    /// The scope this function was created in — the essence of a closure.
    closure: u32,
    /// Receiver bound at call time for `obj.method()` and class instances.
    this: Option<Box<Value>>,
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

#[derive(Clone)]
pub enum Value {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    /// Index into `Interp::funcs`.
    Func(u32),
    /// A built-in implemented in Rust (console.log, document.write, ...).
    Native(&'static str),
    /// Index into `Interp::objects`.
    Object(u32),
    /// Index into `Interp::arrays`.
    Array(u32),
    /// A handle into the document snapshot (see [`super::dom`]).
    Element(usize),
    /// A compiled regular expression (see [`super::regex`]). Never part of a
    /// cycle — a `Regex` holds no `Value` — so it stays a plain `Rc`.
    Regex(Rc<super::regex::Regex>),
}

impl Value {
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

/// What a statement did: fall through, return from the enclosing function, or
/// unwind to the nearest loop. `Break`/`Continue` propagate up through
/// `exec_body` exactly like `Return` does — a block or an `if` doesn't
/// consume them, only a loop does — so a `break` three blocks deep still
/// reaches the loop it means to leave.
enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

/// A JavaScript exception travelling up the stack — the value a script threw
/// itself, or one this interpreter constructs for its own faults. Either way
/// `catch` receives it as-is, the same as a real engine raising a
/// `TypeError`/`ReferenceError` the same way a script's own `throw` does.
pub struct Thrown(pub Value);

/// What `new` recognizes as an error constructor — both the JS-visible name
/// and the `.name` an instance gets, which are the same word.
const ERROR_KINDS: [&str; 5] =
    ["Error", "TypeError", "RangeError", "SyntaxError", "ReferenceError"];

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
    envs: Vec<EnvData>,
    env: u32,
    objects: Vec<RefCell<HashMap<String, Value>>>,
    arrays: Vec<RefCell<Vec<Value>>>,
    funcs: Vec<FuncData>,
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

impl Interp {
    pub fn new() -> Interp {
        Interp::with_dom(DomView::default())
    }

    pub fn with_dom(dom: DomView) -> Interp {
        let mut interp = Interp {
            envs: Vec::new(),
            env: 0,
            objects: Vec::new(),
            arrays: Vec::new(),
            funcs: Vec::new(),
            depth: 0,
            dom,
            handlers: HashMap::new(),
            timers: Vec::new(),
            timer_seq: 0,
            loader: None,
            store: None,
            out: Output::default(),
        };
        let root = interp.new_env(None, true);
        interp.env = root;

        let console = interp.namespace(&[("log", "console.log"), ("error", "console.log")]);
        interp.env_define(root, "console".into(), console);
        interp.env_define(root, "setTimeout".into(), Value::Native("setTimeout"));
        interp.env_define(root, "fetch".into(), Value::Native("fetch"));
        // `new Error(msg)` and friends: constructible via `construct`'s own
        // native-tag case, which is what actually shapes the object — these
        // bindings just make the names resolve to something `new` can call.
        for kind in ERROR_KINDS {
            interp.env_define(root, kind.to_string(), Value::Native(kind));
        }
        let json = interp.namespace(&[("parse", "JSON.parse"), ("stringify", "JSON.stringify")]);
        interp.env_define(root, "JSON".into(), json);
        let promises = interp.namespace(&[
            ("resolve", "Promise.resolve"),
            ("reject", "Promise.reject"),
            ("all", "Promise.all"),
        ]);
        interp.env_define(root, "Promise".into(), promises);
        let local_storage = interp.namespace(&[
            ("getItem", "localStorage.getItem"),
            ("setItem", "localStorage.setItem"),
            ("removeItem", "localStorage.removeItem"),
            ("clear", "localStorage.clear"),
        ]);
        interp.env_define(root, "localStorage".into(), local_storage.clone());
        let document = interp.namespace(&[
            ("write", "document.write"),
            ("getElementById", "document.getElementById"),
            ("querySelector", "document.querySelector"),
            ("querySelectorAll", "document.querySelectorAll"),
            ("getElementsByClassName", "document.getElementsByClassName"),
            ("getElementsByTagName", "document.getElementsByTagName"),
        ]);
        interp.env_define(root, "document".into(), document.clone());

        // `window` is the global object in a browser, and scripts reach for it
        // constantly — feature-detecting on it, or just calling
        // window.addEventListener. Without it they fail at the first mention.
        //
        // ponytail: the objects it holds are the same ones defined above; it is
        // not a live alias, so `window.foo = 1` does not create a global `foo`.
        let console = interp.env_get(root, "console").unwrap_or(Value::Undefined);
        let window_map = HashMap::from([
            ("document".to_string(), document),
            ("console".to_string(), console),
            ("localStorage".to_string(), local_storage),
            ("setTimeout".to_string(), Value::Native("setTimeout")),
            ("fetch".to_string(), Value::Native("fetch")),
            // Listening is accepted and does nothing: the events these ask for
            // (load, resize, scroll) are not dispatched, and pretending to
            // register is better than failing the script outright.
            ("addEventListener".to_string(), Value::Native("window.addEventListener")),
            ("removeEventListener".to_string(), Value::Native("window.addEventListener")),
        ]);
        let window = interp.new_object(window_map);
        interp.env_define(root, "window".into(), window);

        interp
    }

    pub fn run(&mut self, program: &[Stmt]) {
        // Hoist function declarations so they can be called before their definition.
        for stmt in program {
            if let Stmt::FuncDecl { name, params, body } = stmt {
                let f = self.make_function(params.clone(), body.clone());
                self.env_define(self.env, name.clone(), f);
            }
        }
        for stmt in program {
            match self.exec(stmt) {
                Ok(Flow::Return(_)) => break,
                // A stray `break`/`continue` outside any loop is a syntax
                // error in real JS; tolerated here rather than treated as
                // fatal, same as everything else this parser doesn't reject.
                Ok(Flow::Normal | Flow::Break | Flow::Continue) => {}
                Err(e) => {
                    let msg = self.describe(&e);
                    self.out.errors.push(msg);
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
            let msg = self.describe(&e);
            self.out.errors.push(msg);
        }
        true
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
                let msg = self.describe(&e);
                self.out.errors.push(msg);
            }
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

    // ---- environment arena -------------------------------------------------

    fn new_env(&mut self, parent: Option<u32>, is_function_scope: bool) -> u32 {
        self.envs.push(EnvData {
            vars: NameMap::default(),
            parent,
            is_function_scope,
        });
        (self.envs.len() - 1) as u32
    }

    /// An ordinary block scope (`if`, `for`, `{ ... }`) — `var` skips past
    /// these looking for a function boundary; `let`/`const` stop here.
    fn env_child(&mut self, parent: u32) -> u32 {
        self.new_env(Some(parent), false)
    }

    /// A function call's own scope — where its parameters live, and where
    /// `var` declared anywhere in its body (however deeply nested in blocks)
    /// actually ends up.
    fn env_function_child(&mut self, parent: u32) -> u32 {
        self.new_env(Some(parent), true)
    }

    fn env_get(&self, mut id: u32, name: &str) -> Option<Value> {
        loop {
            let e = &self.envs[id as usize];
            if let Some(v) = e.vars.get(name) {
                return Some(v.clone());
            }
            id = e.parent?;
        }
    }

    /// Assign to an existing binding somewhere up the chain; returns false if unbound.
    fn env_set(&mut self, mut id: u32, name: &str, value: Value) -> bool {
        loop {
            let e = &mut self.envs[id as usize];
            // Overwrite in place: re-inserting would allocate a second copy of
            // the name on every assignment, which in a loop is most of the work.
            if let Some(slot) = e.vars.get_mut(name) {
                *slot = value;
                return true;
            }
            match e.parent {
                Some(parent) => id = parent,
                None => return false,
            }
        }
    }

    fn env_define(&mut self, id: u32, name: String, value: Value) {
        self.envs[id as usize].vars.insert(name, value);
    }

    /// `var`'s own binding rule: walk up past every ordinary block scope and
    /// land in the nearest function (or global) scope. `id` itself is where
    /// the *lookup* starts, not necessarily where the value ends up — exactly
    /// what lets `if (x) { var y = 1; }` make `y` visible after the `if`,
    /// which `env_define` alone cannot.
    fn env_define_var(&mut self, mut id: u32, name: String, value: Value) {
        loop {
            let boundary = self.envs[id as usize].is_function_scope;
            if boundary {
                self.envs[id as usize].vars.insert(name, value);
                return;
            }
            match self.envs[id as usize].parent {
                Some(parent) => id = parent,
                // No boundary found before running out of scopes — cannot
                // happen (the root is always one) — fall back to defining
                // here rather than silently dropping the declaration.
                None => {
                    self.envs[id as usize].vars.insert(name, value);
                    return;
                }
            }
        }
    }

    /// Run `body` in a fresh child scope, restoring the previous scope afterwards.
    /// Enter a fresh scope, returning the one to restore afterwards.
    fn push_scope(&mut self) -> u32 {
        let child = self.env_child(self.env);
        std::mem::replace(&mut self.env, child)
    }

    // ---- object / array / function arena -----------------------------------

    fn new_object(&mut self, map: HashMap<String, Value>) -> Value {
        self.objects.push(RefCell::new(map));
        Value::Object((self.objects.len() - 1) as u32)
    }

    fn new_array(&mut self, items: Vec<Value>) -> Value {
        self.arrays.push(RefCell::new(items));
        Value::Array((self.arrays.len() - 1) as u32)
    }

    fn namespace(&mut self, entries: &[(&str, &'static str)]) -> Value {
        let map: HashMap<String, Value> = entries
            .iter()
            .map(|(k, v)| (k.to_string(), Value::Native(v)))
            .collect();
        self.new_object(map)
    }

    fn make_function(&mut self, params: Vec<String>, body: Vec<Stmt>) -> Value {
        self.funcs.push(FuncData {
            params: Rc::new(params),
            body: Rc::new(body),
            closure: self.env,
            this: None,
        });
        Value::Func((self.funcs.len() - 1) as u32)
    }

    /// Same function, but called with `receiver` as `this`.
    fn bind_this(&mut self, f: u32, receiver: Value) -> Value {
        let (params, body, closure) = {
            let fd = &self.funcs[f as usize];
            (fd.params.clone(), fd.body.clone(), fd.closure)
        };
        self.funcs.push(FuncData {
            params,
            body,
            closure,
            this: Some(Box::new(receiver)),
        });
        Value::Func((self.funcs.len() - 1) as u32)
    }

    // ---- promises, errors, display -----------------------------------------

    /// Wrap a value in a settled promise.
    fn promise(&mut self, value: Value, rejected: bool) -> Value {
        let mut map = HashMap::new();
        map.insert(PROMISE_KEY.to_string(), value);
        if rejected {
            map.insert(REJECTED_KEY.to_string(), Value::Bool(true));
        }
        self.new_object(map)
    }

    /// What a value settles to: a promise's contents, or the value itself. A
    /// rejected promise settles by throwing, exactly as `await` would.
    fn settled(&self, value: &Value) -> Result<Value, Thrown> {
        match self.unwrap_promise(value) {
            Some((inner, true)) => Err(Thrown(inner)),
            Some((inner, false)) => Ok(inner),
            None => Ok(value.clone()),
        }
    }

    /// `(settled value, was it a rejection)` if this is a promise.
    fn unwrap_promise(&self, value: &Value) -> Option<(Value, bool)> {
        let Value::Object(id) = value else { return None };
        let map = self.objects[*id as usize].borrow();
        let inner = map.get(PROMISE_KEY)?.clone();
        Some((inner, map.contains_key(REJECTED_KEY)))
    }

    /// An error object shaped like `new Error(message)` produces: `.name` and
    /// `.message`, and nothing else — this engine has no prototype chain for
    /// `instanceof` or `.stack` to hang off yet.
    fn make_error(&mut self, kind: &str, message: impl Into<String>) -> Value {
        let mut map = HashMap::new();
        map.insert("name".to_string(), Value::Str(kind.to_string()));
        map.insert("message".to_string(), Value::Str(message.into()));
        self.new_object(map)
    }

    /// Build the exception this interpreter raises for its own faults — the
    /// same shape `new Error(...)` produces, so `catch (e) { e.message }`
    /// reads the same whether the page threw it or this interpreter did.
    fn err(&mut self, kind: &str, message: impl Into<String>) -> Thrown {
        Thrown(self.make_error(kind, message))
    }

    /// How an uncaught exception reads in `Output.errors` — `name: message`
    /// for one of this interpreter's own error objects, or the value's plain
    /// display otherwise (a script can `throw "a string"` or `throw 42` just
    /// as validly as `throw new Error(...)`).
    fn describe(&self, thrown: &Thrown) -> String {
        if let Value::Object(id) = &thrown.0 {
            let (name, message) = {
                let map = self.objects[*id as usize].borrow();
                (map.get("name").cloned(), map.get("message").cloned())
            };
            if let (Some(name), Some(message)) = (name, message) {
                return format!("{}: {}", self.to_display(&name), self.to_display(&message));
            }
        }
        self.to_display(&thrown.0)
    }

    fn to_display(&self, value: &Value) -> String {
        match value {
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
            Value::Array(id) => self.arrays[*id as usize]
                .borrow()
                .iter()
                .map(|item| self.to_display(item))
                .collect::<Vec<_>>()
                .join(","),
            Value::Object(_) => "[object Object]".into(),
            Value::Element(_) => "[object HTMLElement]".into(),
        }
    }

    // ---- statement/expression execution ------------------------------------

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
    ) -> Result<Flow, Thrown> {
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
            match self.exec(body)? {
                Flow::Return(v) => return Ok(Flow::Return(v)),
                Flow::Break => return Ok(Flow::Normal),
                // `continue` in a `for` loop still runs the step — it skips
                // only the rest of the body, not the increment — so it falls
                // through here rather than returning.
                Flow::Continue | Flow::Normal => {}
            }
            if let Some(step) = step {
                self.eval(step)?;
            }
            guard += 1;
            // A page that loops forever must not take the browser with it.
            if guard > 1_000_000 {
                return Err(self.err("RangeError", "loop iteration limit exceeded"));
            }
        }
    }

    fn exec(&mut self, stmt: &Stmt) -> Result<Flow, Thrown> {
        match stmt {
            Stmt::VarDecl { kind, names } => {
                for (name, init) in names {
                    let value = match init {
                        Some(e) => self.eval(e)?,
                        None => Value::Undefined,
                    };
                    match kind {
                        // Real hoisting: lands in the nearest function (or
                        // global) scope, not the block this `var` happens to
                        // be written in.
                        DeclKind::Var => self.env_define_var(self.env, name.clone(), value),
                        // `let`/`const` stay exactly where they're written —
                        // already what `env_define` does, since a block that
                        // declares anything already gets its own scope.
                        DeclKind::Let | DeclKind::Const => {
                            self.env_define(self.env, name.clone(), value)
                        }
                    }
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
                    match self.exec(body)? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => {}
                    }
                    guard += 1;
                    if guard > 1_000_000 {
                        return Err(self.err("RangeError", "loop iteration limit exceeded"));
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
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            Stmt::FuncDecl { name, params, body } => {
                let f = self.make_function(params.clone(), body.clone());
                self.env_define(self.env, name.clone(), f);
                Ok(Flow::Normal)
            }
            Stmt::Throw(expr) => {
                let v = self.eval(expr)?;
                // The error channel *is* the exception now — no tagging or
                // stringifying needed to tell a `throw` apart from anything
                // else that propagates through `?`, because nothing else does.
                Err(Thrown(v))
            }
            Stmt::Try {
                body,
                param,
                catch,
                finally,
            } => {
                let result = self.exec_body(body);
                let outcome = match result {
                    Err(Thrown(value)) => {
                        let saved = self.env;
                        self.env = self.env_child(saved);
                        if let Some(name) = param {
                            // The real thrown value, not a message reparsed
                            // out of it — `catch (e)` sees whatever was
                            // actually thrown, object or string or number.
                            self.env_define(self.env, name.clone(), value);
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
                    match self.env_get(self.env, parent_name) {
                        Some(Value::Object(base_id)) => {
                            let base = self.objects[base_id as usize].borrow().clone();
                            map.extend(base);
                            parent_table = Some(Value::Object(base_id));
                        }
                        _ => {
                            let msg = format!("{parent_name} is not a class");
                            return Err(self.err("TypeError", msg));
                        }
                    }
                }
                // Methods capture a scope where `super` is *this* class's parent.
                // Resolving it from the instance instead would make an inherited
                // constructor call itself, since the instance's parent is the
                // subclass's parent, not the defining class's.
                let saved = self.env;
                self.env = self.env_child(saved);
                if let Some(parent) = parent_table {
                    self.env_define(self.env, SUPER_KEY.into(), parent);
                }
                for (method, params, body) in methods {
                    let f = self.make_function(params.clone(), body.clone());
                    map.insert(method.clone(), f);
                }
                self.env = saved;
                let class = self.new_object(map);
                self.env_define(self.env, name.clone(), class);
                Ok(Flow::Normal)
            }
        }
    }

    fn exec_body(&mut self, body: &[Stmt]) -> Result<Flow, Thrown> {
        for stmt in body {
            match self.exec(stmt)? {
                Flow::Normal => {}
                // Anything else unwinds the rest of this block — a `return`,
                // `break` or `continue` three statements in means the fourth
                // never runs, whether this block is a loop body, an `if`
                // arm, or nested another level inside either.
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, Thrown> {
        match expr {
            Expr::Num(n) => Ok(Value::Num(*n)),
            Expr::Regex { pattern, flags } => match super::regex::Regex::new(pattern, flags) {
                Some(re) => Ok(Value::Regex(Rc::new(re))),
                // Refusing is better than matching the wrong thing silently.
                None => {
                    let msg = format!("unsupported regular expression /{pattern}/{flags}");
                    Err(self.err("SyntaxError", msg))
                }
            },
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Null),
            Expr::Undefined => Ok(Value::Undefined),
            Expr::Ident(name) => match self.env_get(self.env, name) {
                Some(v) => Ok(v),
                None => {
                    let msg = format!("{name} is not defined");
                    Err(self.err("ReferenceError", msg))
                }
            },
            Expr::Func { params, body } => Ok(self.make_function(params.clone(), body.clone())),
            Expr::This => Ok(self.env_get(self.env, "this").unwrap_or(Value::Undefined)),
            Expr::Super => Ok(self.env_get(self.env, SUPER_KEY).unwrap_or(Value::Undefined)),
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
                Ok(self.new_array(values))
            }
            Expr::ObjectLit(props) => {
                let mut map = HashMap::new();
                for (key, expr) in props {
                    let v = self.eval(expr)?;
                    map.insert(key.clone(), v);
                }
                Ok(self.new_object(map))
            }
            // Nothing to wait for: `await` unwraps a settled promise, and a
            // rejected one throws exactly where the `await` stands.
            Expr::Unary { op, expr } if op == "await" => {
                let value = self.eval(expr)?;
                match self.unwrap_promise(&value) {
                    Some((inner, true)) => Err(Thrown(inner)),
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
                Ok(self.binary_op(op, &l, &r))
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
                    (Value::Array(id), _) => {
                        let i = key.as_number();
                        let items = self.arrays[*id as usize].borrow();
                        if i >= 0.0 && (i as usize) < items.len() {
                            items[i as usize].clone()
                        } else {
                            Value::Undefined
                        }
                    }
                    _ => {
                        let k = self.to_display(&key);
                        self.get_property(&obj, &k)
                    }
                })
            }
            Expr::Call { callee, args } => {
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a)?);
                }
                // `super(...)` calls the parent constructor on the current `this`.
                if matches!(**callee, Expr::Super) {
                    let parent = self.env_get(self.env, SUPER_KEY).unwrap_or(Value::Undefined);
                    let this = self.env_get(self.env, "this").unwrap_or(Value::Undefined);
                    let ctor = self.get_property(&parent, "constructor");
                    if let Value::Func(idx) = ctor {
                        let bound = self.bind_this(idx, this);
                        return self.call(bound, values);
                    }
                    return Ok(Value::Undefined);
                }
                // Method calls need the receiver, so handle `obj.m()` specially.
                if let Expr::Member { object, property } = &**callee {
                    let receiver = self.eval(object)?;
                    // `super.m()` runs the parent's method against the current `this`.
                    if matches!(**object, Expr::Super) {
                        let this = self.env_get(self.env, "this").unwrap_or(Value::Undefined);
                        let method = self.get_property(&receiver, property);
                        if let Value::Func(idx) = method {
                            let bound = self.bind_this(idx, this);
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
                        Value::Func(idx) => self.bind_this(idx, receiver),
                        other => other,
                    };
                    return self.call(f, values);
                }
                let f = self.eval(callee)?;
                self.call(f, values)
            }
        }
    }

    fn assign_to(&mut self, target: &Expr, v: Value) -> Result<(), Thrown> {
        match target {
            Expr::Ident(name) => {
                if !self.env_set(self.env, name, v.clone()) {
                    self.env_define(self.env, name.clone(), v); // implicit global
                }
                Ok(())
            }
            Expr::Member { object, property } => {
                match self.eval(object)? {
                    Value::Object(id) => {
                        self.objects[id as usize]
                            .borrow_mut()
                            .insert(property.clone(), v);
                    }
                    Value::Element(i) => match property.as_str() {
                        // Field text lives in the document's form state, not the DOM.
                        "value" => {
                            let text = self.to_display(&v);
                            if let Some(id) = self.node_id_of(i) {
                                self.out.field_writes.push((id, text.clone()));
                            }
                            self.reflect(i, |e| e.text = text);
                        }
                        // `onclick`, `oninput`, `onchange`, ... all register the same way.
                        name if name.starts_with("on") => {
                            if let Some(id) = self.node_id_of(i) {
                                self.handlers.insert((id, name[2..].to_string()), v);
                            }
                        }
                        "textContent" | "innerText" => {
                            let text = self.to_display(&v);
                            self.out
                                .mutations
                                .push(Mutation::SetText(i, text.clone()));
                            self.reflect(i, |e| e.text = text);
                        }
                        "innerHTML" => {
                            let text = self.to_display(&v);
                            self.out
                                .mutations
                                .push(Mutation::SetHtml(i, text.clone()));
                            self.reflect(i, |e| e.text = text);
                        }
                        // Restyling: swapping the class re-runs the cascade for this node.
                        "className" => {
                            let text = self.to_display(&v);
                            self.out
                                .mutations
                                .push(Mutation::SetClass(i, text.clone()));
                            self.reflect(i, |e| e.class = text);
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
                    Value::Array(id) => {
                        let i = key.as_number();
                        if i >= 0.0 {
                            let mut items = self.arrays[id as usize].borrow_mut();
                            let i = i as usize;
                            if i >= items.len() {
                                items.resize(i + 1, Value::Undefined);
                            }
                            items[i] = v;
                        }
                    }
                    Value::Object(id) => {
                        let key_str = self.to_display(&key);
                        self.objects[id as usize].borrow_mut().insert(key_str, v);
                    }
                    _ => {}
                }
                Ok(())
            }
            _ => Err(self.err("SyntaxError", "invalid assignment target")),
        }
    }

    fn get_property(&self, obj: &Value, property: &str) -> Value {
        match obj {
            Value::Object(id) => self.objects[*id as usize]
                .borrow()
                .get(property)
                .cloned()
                .unwrap_or(Value::Undefined),
            Value::Array(id) if property == "length" => {
                Value::Num(self.arrays[*id as usize].borrow().len() as f64)
            }
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
    ) -> Result<Option<Value>, Thrown> {
        // Promise plumbing first: `.then` on a settled promise is just a call,
        // and a script may chain it on anything an async function returned.
        //
        // ponytail: `async` is parsed and ignored, so a user function's return
        // value is not auto-wrapped. Treating any receiver as already settled
        // keeps `f().then(...)` working; the cost is that `.then` on a plain
        // object calls back with the object instead of failing.
        let is_own_method = matches!(receiver, Value::Object(id) if self.objects[*id as usize].borrow().contains_key(method));
        if matches!(method, "then" | "catch" | "finally") && !is_own_method {
            let (value, rejected) = self.unwrap_promise(receiver).unwrap_or((receiver.clone(), false));
            let handler = match method {
                "then" if !rejected => args.first(),
                "then" => args.get(1), // the second argument is the reject path
                "catch" if rejected => args.first(),
                "finally" => args.first(),
                _ => None,
            };
            let Some(handler) = handler.cloned() else {
                return Ok(Some(self.promise(value, rejected)));
            };
            let call_args = match method {
                "finally" => Vec::new(),
                _ => vec![value.clone()],
            };
            let produced = self.call(handler, call_args)?;
            // `finally` passes the original settlement through untouched.
            return Ok(Some(match method {
                "finally" => self.promise(value, rejected),
                // A handler that returns a promise flattens, as chaining requires.
                _ => match self.unwrap_promise(&produced) {
                    Some((inner, rejected)) => self.promise(inner, rejected),
                    None => self.promise(produced, false),
                },
            }));
        }

        let result = match (receiver, method) {
            // `fetch` responses. Both are settled promises, so `await res.json()`
            // and `res.json().then(...)` both work.
            (Value::Object(id), "text")
                if self.objects[*id as usize].borrow().contains_key(BODY_KEY) =>
            {
                let body = self.objects[*id as usize].borrow()[BODY_KEY].clone();
                self.promise(body, false)
            }
            (Value::Object(id), "json")
                if self.objects[*id as usize].borrow().contains_key(BODY_KEY) =>
            {
                let body_value = self.objects[*id as usize].borrow()[BODY_KEY].clone();
                let body_text = self.to_display(&body_value);
                match self.parse_json(&body_text) {
                    Some(value) => self.promise(value, false),
                    None => self.promise(Value::Str("SyntaxError: bad JSON".into()), true),
                }
            }
            (Value::Array(id), "push") => {
                let id = *id as usize;
                self.arrays[id].borrow_mut().extend(args.iter().cloned());
                Value::Num(self.arrays[id].borrow().len() as f64)
            }
            (Value::Array(id), "pop") => {
                self.arrays[*id as usize].borrow_mut().pop().unwrap_or(Value::Undefined)
            }
            (Value::Array(id), "join") => {
                let sep = match args.first() {
                    Some(v) => self.to_display(v),
                    None => ",".into(),
                };
                let joined = self.arrays[*id as usize]
                    .borrow()
                    .iter()
                    .map(|v| self.to_display(v))
                    .collect::<Vec<_>>()
                    .join(&sep);
                Value::Str(joined)
            }
            (Value::Element(i), "addEventListener") => {
                let event = args.first().map(|v| self.to_display(v));
                if let (Some(event), Some(f), Some(id)) = (event, args.get(1), self.node_id_of(*i))
                {
                    self.handlers.insert((id, event), f.clone());
                }
                Value::Undefined
            }
            // Regex methods, and the string methods that accept one.
            (Value::Regex(re), "test") => {
                let text = args.first().map(|v| self.to_display(v)).unwrap_or_default();
                Value::Bool(re.is_match(&text))
            }
            (Value::Str(s), "replace" | "replaceAll") => match args.first() {
                Some(Value::Regex(re)) => {
                    let with = args.get(1).map(|v| self.to_display(v)).unwrap_or_default();
                    Value::Str(re.replace(s, &with))
                }
                Some(needle) => {
                    let needle = self.to_display(needle);
                    let with = args.get(1).map(|v| self.to_display(v)).unwrap_or_default();
                    Value::Str(match method {
                        "replaceAll" => s.replace(&needle, &with),
                        _ => s.replacen(&needle, &with, 1),
                    })
                }
                None => Value::Str(s.clone()),
            },
            (Value::Str(s), "split") => {
                let parts: Vec<Value> = match args.first() {
                    Some(Value::Regex(re)) => re.split(s).into_iter().map(Value::Str).collect(),
                    Some(sep) => {
                        let sep = self.to_display(sep);
                        s.split(&sep).map(|p| Value::Str(p.to_string())).collect()
                    }
                    None => vec![Value::Str(s.clone())],
                };
                self.new_array(parts)
            }
            (Value::Str(s), "match") => match args.first() {
                Some(Value::Regex(re)) => match re.find(s) {
                    Some((start, end)) => {
                        let hit: String = s.chars().skip(start).take(end - start).collect();
                        self.new_array(vec![Value::Str(hit)])
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
    fn construct(&mut self, class: Value, args: Vec<Value>) -> Result<Value, Thrown> {
        if let Value::Native(kind) = class {
            if ERROR_KINDS.contains(&kind) {
                let message = args.first().map(|v| self.to_display(v)).unwrap_or_default();
                return Ok(self.make_error(kind, message));
            }
        }
        let methods: HashMap<String, Value> = match class {
            Value::Object(id) => self.objects[id as usize].borrow().clone(),
            other => {
                let msg = format!("{} is not a constructor", self.to_display(&other));
                return Err(self.err("TypeError", msg));
            }
        };
        let instance = self.new_object(HashMap::new());
        let Value::Object(instance_id) = instance else {
            unreachable!()
        };
        for (name, method) in &methods {
            let bound = match method {
                Value::Func(idx) => self.bind_this(*idx, instance.clone()),
                other => other.clone(),
            };
            self.objects[instance_id as usize]
                .borrow_mut()
                .insert(name.clone(), bound);
        }
        if let Some(ctor) = methods.get("constructor") {
            if let Value::Func(idx) = ctor {
                let bound = self.bind_this(*idx, instance.clone());
                self.call(bound, args)?;
            }
        }
        Ok(instance)
    }

    fn call(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, Thrown> {
        match callee {
            Value::Native(name) => {
                let text = args
                    .iter()
                    .map(|v| self.to_display(v))
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
                        let resp = self.new_object(response);
                        return Ok(self.promise(resp, false));
                    }
                    "Promise.resolve" => {
                        let v = args.first().cloned().unwrap_or(Value::Undefined);
                        return Ok(self.promise(v, false));
                    }
                    "Promise.reject" => {
                        let v = args.first().cloned().unwrap_or(Value::Undefined);
                        return Ok(self.promise(v, true));
                    }
                    // Every promise here is already settled, so "all of them" is
                    // just their values in order.
                    "Promise.all" => {
                        let items = match args.first() {
                            Some(Value::Array(id)) => self.arrays[*id as usize].borrow().clone(),
                            _ => Vec::new(),
                        };
                        let mut out = Vec::with_capacity(items.len());
                        for item in items {
                            out.push(self.settled(&item)?);
                        }
                        let arr = self.new_array(out);
                        return Ok(self.promise(arr, false));
                    }
                    "JSON.parse" => {
                        let text = args.first().map(|v| self.to_display(v)).unwrap_or_default();
                        return match self.parse_json(&text) {
                            Some(v) => Ok(v),
                            None => Err(self.err("SyntaxError", "bad JSON")),
                        };
                    }
                    "JSON.stringify" => {
                        let s = match args.first() {
                            Some(v) => self.stringify_json(v),
                            None => String::new(),
                        };
                        return Ok(Value::Str(s));
                    }
                    "localStorage.getItem" => {
                        let key = args.first().map(|v| self.to_display(v)).unwrap_or_default();
                        return Ok(match self.store.as_ref().and_then(|s| s.get(&key)) {
                            Some(v) => Value::Str(v),
                            None => Value::Null, // absent keys read as null, like the web
                        });
                    }
                    "localStorage.setItem" => {
                        if let Some(key) = args.first() {
                            let key = self.to_display(key);
                            let value = args.get(1).map(|v| self.to_display(v)).unwrap_or_default();
                            if let Some(store) = &self.store {
                                store.set(&key, &value);
                            }
                        }
                        return Ok(Value::Undefined);
                    }
                    "localStorage.removeItem" => {
                        if let Some(key) = args.first() {
                            let key = self.to_display(key);
                            if let Some(store) = &self.store {
                                store.remove(&key);
                            }
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
                        return Ok(self.new_array(found));
                    }
                    "document.getElementById" => {
                        return Ok(match self.dom.find_by_id(&text) {
                            Some(i) => Value::Element(i),
                            None => Value::Null,
                        })
                    }
                    _ => {
                        let msg = format!("unknown builtin {name}");
                        return Err(self.err("TypeError", msg));
                    }
                }
                Ok(Value::Undefined)
            }
            Value::Func(f) => {
                if self.depth > 200 {
                    return Err(self.err("RangeError", "maximum call depth exceeded"));
                }
                // Calls run in a child of the *defining* scope, not the calling one.
                let (closure, this, params, body) = {
                    let fd = &self.funcs[f as usize];
                    (fd.closure, fd.this.clone(), fd.params.clone(), fd.body.clone())
                };
                let saved = self.env;
                // A function boundary: where this call's own `var`s land,
                // however many blocks deep inside the body they're written.
                self.env = self.env_function_child(closure);
                if let Some(receiver) = this {
                    self.env_define(self.env, "this".into(), *receiver);
                }
                for (i, param) in params.iter().enumerate() {
                    let v = args.get(i).cloned().unwrap_or(Value::Undefined);
                    self.env_define(self.env, param.clone(), v);
                }
                self.depth += 1;
                let result = self.exec_body(&body);
                self.depth -= 1;
                self.env = saved;
                match result? {
                    Flow::Return(v) => Ok(v),
                    // A `break`/`continue` that reached here without a loop
                    // to catch it is invalid JS this parser doesn't reject;
                    // tolerated the same way a function simply ending is.
                    Flow::Normal | Flow::Break | Flow::Continue => Ok(Value::Undefined),
                }
            }
            other => {
                let msg = format!("{} is not a function", self.to_display(&other));
                Err(self.err("TypeError", msg))
            }
        }
    }

    // ---- JSON -----------------------------------------------------------

    fn stringify_json(&self, value: &Value) -> String {
        match value {
            Value::Num(_) => self.to_display(value),
            Value::Bool(b) => b.to_string(),
            Value::Null | Value::Undefined => "null".to_string(),
            Value::Str(s) => quote_json(s),
            Value::Array(id) => {
                let parts: Vec<String> = self.arrays[*id as usize]
                    .borrow()
                    .iter()
                    .map(|v| self.stringify_json(v))
                    .collect();
                format!("[{}]", parts.join(","))
            }
            Value::Object(id) => {
                // Sorted, because a HashMap has no order of its own and a stringify
                // that shuffled its keys between runs would be untestable.
                let map = self.objects[*id as usize].borrow();
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let parts: Vec<String> = keys
                    .iter()
                    .map(|k| format!("{}:{}", quote_json(k), self.stringify_json(&map[*k])))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
            // Functions and elements have no JSON form; a browser drops them.
            _ => "null".to_string(),
        }
    }

    /// A JSON reader. Deliberately its own parser rather than the JS one: JSON is
    /// a data format from untrusted servers, and it must not accept expressions.
    fn parse_json(&mut self, text: &str) -> Option<Value> {
        let mut chars: Vec<char> = text.chars().collect();
        chars.push('\0'); // sentinel, so peeking past the end is not a special case
        let mut pos = 0;
        let value = self.json_value(&chars, &mut pos)?;
        json_space(&chars, &mut pos);
        match chars[pos] {
            '\0' => Some(value),
            _ => None, // trailing junk: not one JSON document
        }
    }

    fn json_value(&mut self, chars: &[char], pos: &mut usize) -> Option<Value> {
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
                        return Some(self.new_array(items));
                    }
                    items.push(self.json_value(chars, pos)?);
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
                        return Some(self.new_object(map));
                    }
                    let key = json_string(chars, pos)?;
                    json_space(chars, pos);
                    if chars[*pos] != ':' {
                        return None;
                    }
                    *pos += 1;
                    let v = self.json_value(chars, pos)?;
                    map.insert(key, v);
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

    fn binary_op(&self, op: &str, l: &Value, r: &Value) -> Value {
        match op {
            // `+` concatenates if either side is a string, like JS.
            "+" => match (l, r) {
                (Value::Str(_), _) | (_, Value::Str(_)) => {
                    Value::Str(format!("{}{}", self.to_display(l), self.to_display(r)))
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

fn json_space(chars: &[char], pos: &mut usize) {
    while chars[*pos].is_whitespace() {
        *pos += 1;
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
