//! Tree-walking evaluator.
//!
//! ponytail: a straightforward AST interpreter — no bytecode VM, inline caches, or
//! JIT (those are Phase 2/3 in docs/01-ARCHITECTURE.md §4). Scopes are a simple
//! stack, so functions see globals and their own locals but do NOT capture their
//! defining environment: real closures need an environment chain.

use super::parser::{Expr, Stmt};
use std::collections::HashMap;
use std::rc::Rc;

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
    /// A namespace object like `console`; property -> value.
    Object(Rc<HashMap<String, Value>>),
}

pub struct FuncData {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
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
            Value::Object(_) => "[object]".into(),
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

    fn as_number(&self) -> f64 {
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

#[derive(Default)]
pub struct Output {
    pub console: Vec<String>,
    /// HTML emitted by `document.write`, appended to the document before layout.
    pub writes: String,
    pub errors: Vec<String>,
}

pub struct Interp {
    scopes: Vec<HashMap<String, Value>>,
    pub out: Output,
}

impl Interp {
    pub fn new() -> Interp {
        let mut globals = HashMap::new();
        globals.insert(
            "console".to_string(),
            Value::Object(Rc::new(HashMap::from([(
                "log".to_string(),
                Value::Native("console.log"),
            )]))),
        );
        globals.insert(
            "document".to_string(),
            Value::Object(Rc::new(HashMap::from([(
                "write".to_string(),
                Value::Native("document.write"),
            )]))),
        );
        Interp { scopes: vec![globals], out: Output::default() }
    }

    pub fn run(&mut self, program: &[Stmt]) {
        // Hoist function declarations so they can be called before their definition.
        for stmt in program {
            if let Stmt::FuncDecl { name, params, body } = stmt {
                let f = Value::Func(Rc::new(FuncData { params: params.clone(), body: body.clone() }));
                self.define(name.clone(), f);
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

    fn define(&mut self, name: String, value: Value) {
        self.scopes.last_mut().expect("a scope always exists").insert(name, value);
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }

    fn assign(&mut self, name: &str, value: Value) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return;
            }
        }
        self.define(name.to_string(), value); // implicit global
    }

    fn exec(&mut self, stmt: &Stmt) -> Result<Flow, String> {
        match stmt {
            Stmt::VarDecl { name, init } => {
                let v = match init {
                    Some(e) => self.eval(e)?,
                    None => Value::Undefined,
                };
                self.define(name.clone(), v);
                Ok(Flow::Normal)
            }
            Stmt::ExprStmt(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::Block(body) => {
                self.scopes.push(HashMap::new());
                let result = self.exec_body(body);
                self.scopes.pop();
                result
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
                self.scopes.push(HashMap::new());
                let result = (|| {
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
                            break;
                        }
                        if let Flow::Return(v) = self.exec(body)? {
                            return Ok(Flow::Return(v));
                        }
                        if let Some(step) = step {
                            self.eval(step)?;
                        }
                        guard += 1;
                        if guard > 1_000_000 {
                            return Err("loop iteration limit exceeded".into());
                        }
                    }
                    Ok(Flow::Normal)
                })();
                self.scopes.pop();
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
                let f = Value::Func(Rc::new(FuncData { params: params.clone(), body: body.clone() }));
                self.define(name.clone(), f);
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
                self.lookup(name).ok_or_else(|| format!("{name} is not defined"))
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
                match &**target {
                    Expr::Ident(name) => {
                        self.assign(name, v.clone());
                        Ok(v)
                    }
                    _ => Err("invalid assignment target".into()),
                }
            }
            Expr::Member { object, property } => {
                let obj = self.eval(object)?;
                Ok(match obj {
                    Value::Object(map) => map.get(property).cloned().unwrap_or(Value::Undefined),
                    Value::Str(s) if property == "length" => Value::Num(s.chars().count() as f64),
                    _ => Value::Undefined,
                })
            }
            Expr::Call { callee, args } => {
                let f = self.eval(callee)?;
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a)?);
                }
                self.call(f, values)
            }
        }
    }

    fn call(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, String> {
        match callee {
            Value::Native(name) => {
                let text =
                    args.iter().map(Value::to_display).collect::<Vec<_>>().join(" ");
                match name {
                    "console.log" => self.out.console.push(text),
                    "document.write" => self.out.writes.push_str(&text),
                    _ => return Err(format!("unknown builtin {name}")),
                }
                Ok(Value::Undefined)
            }
            Value::Func(f) => {
                let mut scope = HashMap::new();
                for (i, param) in f.params.iter().enumerate() {
                    scope.insert(param.clone(), args.get(i).cloned().unwrap_or(Value::Undefined));
                }
                if self.scopes.len() > 200 {
                    return Err("maximum call depth exceeded".into());
                }
                self.scopes.push(scope);
                let result = self.exec_body(&f.body);
                self.scopes.pop();
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
