//! Tree-walking evaluator.
//!
//! Scopes form an environment chain, so a function captures the scope it was
//! defined in — that is what makes closures work.
//!
//! ponytail: a straightforward AST interpreter — no bytecode VM, inline caches, or
//! JIT (those are Phase 2/3 in docs/01-ARCHITECTURE.md §4). No prototypes: objects
//! are plain maps and only a handful of built-in methods exist.

use super::dom::{DomView, Mutation};
use crate::resource::ResourceLoader;
use super::parser::{Expr, Stmt};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

type EnvRef = Rc<RefCell<Env>>;

/// Marks an error as a JS `throw` (catchable) rather than an engine failure.
const THROW_TAG: &str = "\u{1}throw\u{1}";

/// Hidden slot holding a class's parent method table, for `super`.
const SUPER_KEY: &str = "\u{1}super";

/// One lexical scope, linked to the scope enclosing it.
pub struct Env {
    vars: HashMap<String, Value>,
    parent: Option<EnvRef>,
}

impl Env {
    fn root() -> EnvRef {
        Rc::new(RefCell::new(Env { vars: HashMap::new(), parent: None }))
    }

    fn child(parent: &EnvRef) -> EnvRef {
        Rc::new(RefCell::new(Env { vars: HashMap::new(), parent: Some(parent.clone()) }))
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
        if e.vars.contains_key(name) {
            e.vars.insert(name.to_string(), value);
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
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".into(),
            Value::Undefined => "undefined".into(),
            Value::Func(_) | Value::Native(_) => "function".into(),
            Value::Array(items) => {
                items.borrow().iter().map(Value::to_display).collect::<Vec<_>>().join(",")
            }
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
    pub out: Output,
}

fn namespace(entries: &[(&str, &'static str)]) -> Value {
    let map: HashMap<String, Value> =
        entries.iter().map(|(k, v)| (k.to_string(), Value::Native(v))).collect();
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
            "document".into(),
            namespace(&[
                ("write", "document.write"),
                ("getElementById", "document.getElementById"),
            ]),
        );
        Interp {
            env,
            depth: 0,
            dom,
            handlers: HashMap::new(),
            timers: Vec::new(),
            timer_seq: 0,
            loader: None,
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

    fn node_id_of(&self, index: usize) -> Option<usize> {
        self.dom.elements.get(index).map(|e| e.node_id)
    }

    fn make_function(&self, params: Vec<String>, body: Vec<Stmt>) -> Value {
        Value::Func(Rc::new(FuncData { params, body, closure: self.env.clone(), this: None }))
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
    fn in_child_scope(
        &mut self,
        run: impl FnOnce(&mut Self) -> Result<Flow, String>,
    ) -> Result<Flow, String> {
        let saved = self.env.clone();
        self.env = Env::child(&saved);
        let result = run(self);
        self.env = saved;
        result
    }

    fn exec(&mut self, stmt: &Stmt) -> Result<Flow, String> {
        match stmt {
            Stmt::VarDecl { name, init } => {
                let v = match init {
                    Some(e) => self.eval(e)?,
                    None => Value::Undefined,
                };
                Env::define(&self.env, name.clone(), v);
                Ok(Flow::Normal)
            }
            Stmt::ExprStmt(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::Block(body) => {
                let body = body.clone();
                self.in_child_scope(|me| me.exec_body(&body))
            }
            Stmt::If { cond, then, otherwise } => {
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
            Stmt::For { init, cond, step, body } => {
                let (init, cond, step, body) =
                    (init.clone(), cond.clone(), step.clone(), body.clone());
                self.in_child_scope(move |me| {
                    if let Some(init) = &init {
                        me.exec(init)?;
                    }
                    let mut guard = 0;
                    loop {
                        let keep_going = match &cond {
                            Some(c) => me.eval(c)?.truthy(),
                            None => true,
                        };
                        if !keep_going {
                            break;
                        }
                        if let Flow::Return(v) = me.exec(&body)? {
                            return Ok(Flow::Return(v));
                        }
                        if let Some(step) = &step {
                            me.eval(step)?;
                        }
                        guard += 1;
                        if guard > 1_000_000 {
                            return Err("loop iteration limit exceeded".into());
                        }
                    }
                    Ok(Flow::Normal)
                })
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
            Stmt::Try { body, param, catch, finally } => {
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
            Stmt::ClassDecl { name, parent, methods } => {
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
                    map.insert(method.clone(), self.make_function(params.clone(), body.clone()));
                }
                self.env = saved;
                Env::define(&self.env, name.clone(), Value::Object(Rc::new(RefCell::new(map))));
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
            Expr::Ternary { cond, then, otherwise } => {
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
            Expr::Unary { op, expr } => {
                let v = self.eval(expr)?;
                Ok(match op.as_str() {
                    "!" => Value::Bool(!v.truthy()),
                    "-" => Value::Num(-v.as_number()),
                    _ => Value::Num(v.as_number()),
                })
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
                        }
                        // `onclick`, `oninput`, `onchange`, ... all register the same way.
                        name if name.starts_with("on") => {
                            if let Some(id) = self.node_id_of(i) {
                                self.handlers.insert((id, name[2..].to_string()), v);
                            }
                        }
                        "textContent" | "innerText" => {
                            self.out.mutations.push(Mutation::SetText(i, v.to_display()))
                        }
                        "innerHTML" => {
                            self.out.mutations.push(Mutation::SetHtml(i, v.to_display()))
                        }
                        // Restyling: swapping the class re-runs the cascade for this node.
                        "className" => {
                            self.out.mutations.push(Mutation::SetClass(i, v.to_display()))
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
            Value::Object(map) => map.borrow().get(property).cloned().unwrap_or(Value::Undefined),
            Value::Array(items) if property == "length" => {
                Value::Num(items.borrow().len() as f64)
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
    ) -> Result<Option<Value>, String> {
        let result = match (receiver, method) {
            (Value::Array(items), "push") => {
                items.borrow_mut().extend(args.iter().cloned());
                Value::Num(items.borrow().len() as f64)
            }
            (Value::Array(items), "pop") => items.borrow_mut().pop().unwrap_or(Value::Undefined),
            (Value::Array(items), "join") => {
                let sep = args.first().map(Value::to_display).unwrap_or_else(|| ",".into());
                let joined =
                    items.borrow().iter().map(Value::to_display).collect::<Vec<_>>().join(&sep);
                Value::Str(joined)
            }
            (Value::Element(i), "addEventListener") => {
                let event = args.first().map(Value::to_display);
                if let (Some(event), Some(f), Some(id)) =
                    (event, args.get(1), self.node_id_of(*i))
                {
                    self.handlers.insert((id, event), f.clone());
                }
                Value::Undefined
            }
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
                let text = args.iter().map(Value::to_display).collect::<Vec<_>>().join(" ");
                match name {
                    "console.log" => self.out.console.push(text),
                    "document.write" => self.out.writes.push_str(&text),
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
                        // ponytail: synchronous, and returns {ok, status, text} rather
                        // than a Promise — there is no event loop to resolve one yet.
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
                        response.insert("text".into(), Value::Str(body.unwrap_or_default()));
                        return Ok(Value::Object(Rc::new(RefCell::new(response))));
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
        _ => Value::Undefined,
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
