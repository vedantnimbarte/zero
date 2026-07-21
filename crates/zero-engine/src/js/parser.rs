//! JavaScript parser: tokens -> AST, via recursive descent with precedence climbing.
//!
//! ponytail: supports declarations, assignment, calls, member/index access, object
//! and array literals, if/while/for, and function declarations/expressions. No
//! arrow functions, classes, `new`, try/catch, or destructuring yet.

use super::lexer::{Kw, Tok};

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    Unary {
        op: String,
        expr: Box<Expr>,
    },
    Binary {
        op: String,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Assign {
        target: Box<Expr>,
        value: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Member {
        object: Box<Expr>,
        property: String,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    ObjectLit(Vec<(String, Expr)>),
    ArrayLit(Vec<Expr>),
    /// A function expression, which captures the scope it was created in.
    Func {
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    This,
    New {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `super` — resolves to the parent class's method table.
    Super,
    Ternary {
        cond: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
}

#[derive(Debug, Clone)]
pub enum Stmt {
    VarDecl {
        name: String,
        init: Option<Expr>,
    },
    ExprStmt(Expr),
    If {
        cond: Expr,
        then: Box<Stmt>,
        otherwise: Option<Box<Stmt>>,
    },
    While {
        cond: Expr,
        body: Box<Stmt>,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Expr>,
        body: Box<Stmt>,
    },
    Block(Vec<Stmt>),
    Return(Option<Expr>),
    FuncDecl {
        name: String,
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    Throw(Expr),
    Try {
        body: Vec<Stmt>,
        param: Option<String>,
        catch: Vec<Stmt>,
        finally: Vec<Stmt>,
    },
    /// `class C extends P { constructor(..){} method(..){} }`
    ClassDecl {
        name: String,
        parent: Option<String>,
        methods: Vec<(String, Vec<String>, Vec<Stmt>)>,
    },
}

/// Binding power for binary operators; higher binds tighter.
fn precedence(op: &str) -> Option<u8> {
    Some(match op {
        "||" | "??" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" | "===" | "!==" => 6,
        "<" | ">" | "<=" | ">=" => 7,
        "<<" | ">>" | ">>>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        "**" => 11,
        _ => return None,
    })
}

pub fn parse(tokens: Vec<Tok>) -> Result<Vec<Stmt>, String> {
    Parser {
        toks: tokens,
        pos: 0,
    }
    .parse_program()
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        self.pos += 1;
        t
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Tok::Op(o) if o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(format!("expected {op:?}, found {:?}", self.peek()))
        }
    }

    fn eat_kw(&mut self, kw: Kw) -> bool {
        if matches!(self.peek(), Tok::Kw(k) if *k == kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        while *self.peek() != Tok::Eof {
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        while self.eat_op(";") {} // stray separators

        if self.eat_op("{") {
            let mut body = Vec::new();
            while !self.eat_op("}") {
                if *self.peek() == Tok::Eof {
                    return Err("unterminated block".into());
                }
                body.push(self.parse_stmt()?);
            }
            return Ok(Stmt::Block(body));
        }
        if self.eat_kw(Kw::Var) {
            let name = self.expect_ident()?;
            let init = if self.eat_op("=") {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.eat_op(";");
            return Ok(Stmt::VarDecl { name, init });
        }
        if self.eat_kw(Kw::Function) {
            let name = self.expect_ident()?;
            let params = self.parse_params()?;
            let body = self.parse_block_body()?;
            return Ok(Stmt::FuncDecl { name, params, body });
        }
        if self.eat_kw(Kw::Throw) {
            let value = self.parse_expr()?;
            self.eat_op(";");
            return Ok(Stmt::Throw(value));
        }
        if self.eat_kw(Kw::Try) {
            let body = self.parse_block_body()?;
            let (mut param, mut catch) = (None, Vec::new());
            if self.eat_kw(Kw::Catch) {
                if self.eat_op("(") {
                    param = Some(self.expect_ident()?);
                    self.expect_op(")")?;
                }
                catch = self.parse_block_body()?;
            }
            let finally = if self.eat_kw(Kw::Finally) {
                self.parse_block_body()?
            } else {
                Vec::new()
            };
            return Ok(Stmt::Try {
                body,
                param,
                catch,
                finally,
            });
        }
        if self.eat_kw(Kw::Class) {
            let name = self.expect_ident()?;
            let parent = if self.eat_kw(Kw::Extends) {
                Some(self.expect_ident()?)
            } else {
                None
            };
            self.expect_op("{")?;
            let mut methods = Vec::new();
            while !self.eat_op("}") {
                if *self.peek() == Tok::Eof {
                    return Err("unterminated class body".into());
                }
                let method = self.expect_ident()?;
                let params = self.parse_params()?;
                let body = self.parse_block_body()?;
                methods.push((method, params, body));
            }
            return Ok(Stmt::ClassDecl {
                name,
                parent,
                methods,
            });
        }
        if self.eat_kw(Kw::Return) {
            let value = if matches!(self.peek(), Tok::Op(o) if o == ";") || *self.peek() == Tok::Eof
            {
                None
            } else {
                Some(self.parse_expr()?)
            };
            self.eat_op(";");
            return Ok(Stmt::Return(value));
        }
        if self.eat_kw(Kw::If) {
            self.expect_op("(")?;
            let cond = self.parse_expr()?;
            self.expect_op(")")?;
            let then = Box::new(self.parse_stmt()?);
            let otherwise = if self.eat_kw(Kw::Else) {
                Some(Box::new(self.parse_stmt()?))
            } else {
                None
            };
            return Ok(Stmt::If {
                cond,
                then,
                otherwise,
            });
        }
        if self.eat_kw(Kw::While) {
            self.expect_op("(")?;
            let cond = self.parse_expr()?;
            self.expect_op(")")?;
            let body = Box::new(self.parse_stmt()?);
            return Ok(Stmt::While { cond, body });
        }
        if self.eat_kw(Kw::For) {
            self.expect_op("(")?;
            let init = if self.eat_op(";") {
                None
            } else {
                Some(Box::new(self.parse_stmt()?))
            };
            let cond = if self.eat_op(";") {
                None
            } else {
                let c = self.parse_expr()?;
                self.expect_op(";")?;
                Some(c)
            };
            let step = if matches!(self.peek(), Tok::Op(o) if o == ")") {
                None
            } else {
                Some(self.parse_expr()?)
            };
            self.expect_op(")")?;
            let body = Box::new(self.parse_stmt()?);
            return Ok(Stmt::For {
                init,
                cond,
                step,
                body,
            });
        }

        let expr = self.parse_expr()?;
        self.eat_op(";");
        Ok(Stmt::ExprStmt(expr))
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Ident(name) => Ok(name),
            other => Err(format!("expected identifier, found {other:?}")),
        }
    }

    fn parse_params(&mut self) -> Result<Vec<String>, String> {
        self.expect_op("(")?;
        let mut params = Vec::new();
        while !self.eat_op(")") {
            params.push(self.expect_ident()?);
            self.eat_op(",");
        }
        Ok(params)
    }

    fn parse_block_body(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect_op("{")?;
        let mut body = Vec::new();
        while !self.eat_op("}") {
            if *self.peek() == Tok::Eof {
                return Err("unterminated function body".into());
            }
            body.push(self.parse_stmt()?);
        }
        Ok(body)
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<Expr, String> {
        let left = self.parse_ternary()?;
        // Compound assignment desugars to `x = x op v`.
        for (op, bin) in [
            ("=", ""),
            ("+=", "+"),
            ("-=", "-"),
            ("*=", "*"),
            ("/=", "/"),
        ] {
            if matches!(self.peek(), Tok::Op(o) if o == op) {
                self.pos += 1;
                let value = self.parse_assign()?;
                let value = if bin.is_empty() {
                    value
                } else {
                    Expr::Binary {
                        op: bin.to_string(),
                        left: Box::new(left.clone()),
                        right: Box::new(value),
                    }
                };
                return Ok(Expr::Assign {
                    target: Box::new(left),
                    value: Box::new(value),
                });
            }
        }
        Ok(left)
    }

    /// `cond ? a : b`, which binds tighter than assignment but looser than any
    /// binary operator.
    fn parse_ternary(&mut self) -> Result<Expr, String> {
        let cond = self.parse_binary(0)?;
        if !self.eat_op("?") {
            return Ok(cond);
        }
        let then = self.parse_assign()?;
        self.expect_op(":")?;
        let otherwise = self.parse_assign()?;
        Ok(Expr::Ternary {
            cond: Box::new(cond),
            then: Box::new(then),
            otherwise: Box::new(otherwise),
        })
    }

    fn parse_binary(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Op(o) => o.clone(),
                _ => break,
            };
            let bp = match precedence(&op) {
                Some(bp) if bp >= min_bp => bp,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_binary(bp + 1)?;
            left = Expr::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        for op in ["!", "-", "+", "~"] {
            if matches!(self.peek(), Tok::Op(o) if o == op) {
                self.pos += 1;
                let expr = self.parse_unary()?;
                return Ok(Expr::Unary {
                    op: op.to_string(),
                    expr: Box::new(expr),
                });
            }
        }
        // `++x` / `x++` both desugar to `x = x + 1` (value-of-expression ignored).
        if self.eat_op("++") || self.eat_op("--") {
            let target = self.parse_unary()?;
            return Ok(Expr::Assign {
                target: Box::new(target.clone()),
                value: Box::new(Expr::Binary {
                    op: "+".into(),
                    left: Box::new(target),
                    right: Box::new(Expr::Num(1.0)),
                }),
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat_op(".") {
                let property = self.expect_ident()?;
                expr = Expr::Member {
                    object: Box::new(expr),
                    property,
                };
            } else if self.eat_op("[") {
                let index = self.parse_expr()?;
                self.expect_op("]")?;
                expr = Expr::Index {
                    object: Box::new(expr),
                    index: Box::new(index),
                };
            } else if self.eat_op("(") {
                let mut args = Vec::new();
                while !self.eat_op(")") {
                    args.push(self.parse_expr()?);
                    self.eat_op(",");
                }
                expr = Expr::Call {
                    callee: Box::new(expr),
                    args,
                };
            } else if matches!(self.peek(), Tok::Op(o) if o == "++" || o == "--") {
                let op = if matches!(self.peek(), Tok::Op(o) if o == "++") {
                    "+"
                } else {
                    "-"
                };
                self.pos += 1;
                expr = Expr::Assign {
                    target: Box::new(expr.clone()),
                    value: Box::new(Expr::Binary {
                        op: op.into(),
                        left: Box::new(expr),
                        right: Box::new(Expr::Num(1.0)),
                    }),
                };
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Ident(name) => Ok(Expr::Ident(name)),
            Tok::Kw(Kw::True) => Ok(Expr::Bool(true)),
            Tok::Kw(Kw::False) => Ok(Expr::Bool(false)),
            Tok::Kw(Kw::Null) => Ok(Expr::Null),
            Tok::Kw(Kw::Undefined) => Ok(Expr::Undefined),
            Tok::Kw(Kw::This) => Ok(Expr::This),
            Tok::Kw(Kw::Super) => Ok(Expr::Super),
            Tok::Kw(Kw::New) => {
                let callee = self.parse_primary()?;
                let mut args = Vec::new();
                if self.eat_op("(") {
                    while !self.eat_op(")") {
                        args.push(self.parse_expr()?);
                        self.eat_op(",");
                    }
                }
                Ok(Expr::New {
                    callee: Box::new(callee),
                    args,
                })
            }
            Tok::Op(op) if op == "(" => {
                let e = self.parse_expr()?;
                self.expect_op(")")?;
                Ok(e)
            }
            Tok::Op(op) if op == "[" => {
                let mut items = Vec::new();
                while !self.eat_op("]") {
                    items.push(self.parse_expr()?);
                    self.eat_op(",");
                }
                Ok(Expr::ArrayLit(items))
            }
            // In expression position `{` is an object literal; statements handle blocks.
            Tok::Op(op) if op == "{" => {
                let mut props = Vec::new();
                while !self.eat_op("}") {
                    let key = match self.next() {
                        Tok::Ident(k) => k,
                        Tok::Str(k) => k,
                        Tok::Num(n) => n.to_string(),
                        other => return Err(format!("bad object key {other:?}")),
                    };
                    self.expect_op(":")?;
                    props.push((key, self.parse_expr()?));
                    self.eat_op(",");
                }
                Ok(Expr::ObjectLit(props))
            }
            Tok::Kw(Kw::Function) => {
                if matches!(self.peek(), Tok::Ident(_)) {
                    self.pos += 1; // optional name on a function expression
                }
                let params = self.parse_params()?;
                let body = self.parse_block_body()?;
                Ok(Expr::Func { params, body })
            }
            other => Err(format!("unexpected token {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::lexer::tokenize;
    use super::*;

    #[test]
    fn parses_precedence_correctly() {
        let ast = parse(tokenize("var x = 1 + 2 * 3;").unwrap()).unwrap();
        // Should nest as 1 + (2 * 3), not (1 + 2) * 3.
        match &ast[0] {
            Stmt::VarDecl {
                init: Some(Expr::Binary { op, right, .. }),
                ..
            } => {
                assert_eq!(op, "+");
                assert!(matches!(**right, Expr::Binary { .. }));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_literals_and_indexing() {
        let ast = parse(tokenize("var a = [1, 2]; var o = {x: 1}; a[0]; o.x;").unwrap()).unwrap();
        assert!(matches!(
            ast[0],
            Stmt::VarDecl {
                init: Some(Expr::ArrayLit(_)),
                ..
            }
        ));
        assert!(matches!(
            ast[1],
            Stmt::VarDecl {
                init: Some(Expr::ObjectLit(_)),
                ..
            }
        ));
        assert!(matches!(ast[2], Stmt::ExprStmt(Expr::Index { .. })));
        assert!(matches!(ast[3], Stmt::ExprStmt(Expr::Member { .. })));
    }

    #[test]
    fn parses_function_and_call() {
        let ast = parse(tokenize("function f(a){return a;} f(1);").unwrap()).unwrap();
        assert!(matches!(ast[0], Stmt::FuncDecl { .. }));
        assert!(matches!(ast[1], Stmt::ExprStmt(Expr::Call { .. })));
    }
}
