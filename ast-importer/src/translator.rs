
use syntax::ast;
use syntax::ast::*;
use syntax::tokenstream::{TokenStream};
use syntax::parse::token::{DelimToken,Token};
use syntax::abi::Abi;
use renamer::Renamer;
use convert_type::TypeConverter;
use loops::*;
use idiomize::ast_manip::make_ast::*;
use c_ast;
use c_ast::*;
use syntax::ptr::*;
use syntax::print::pprust::*;
use std::ops::Index;
use std::cell::RefCell;
use dtoa;

pub struct Translation {
    pub items: Vec<P<Item>>,
    type_converter: TypeConverter,
    pub ast_context: TypedAstContext,
    renamer: RefCell<Renamer<String>>,
    loops: RefCell<LoopContext>,
}

pub struct WithStmts<T> {
    stmts: Vec<Stmt>,
    val: T,
}

impl<T> WithStmts<T> {
    pub fn new(val: T) -> Self {
        WithStmts { stmts: vec![], val, }
    }
    pub fn and_then<U,F: FnOnce(T) -> WithStmts<U>>(self, f : F) -> WithStmts<U> {
        let mut next = f(self.val);
        let mut stmts = self.stmts;
        stmts.append(&mut next.stmts);
        WithStmts {
            val: next.val,
            stmts
        }
    }
    pub fn map<U,F: FnOnce(T) -> U>(self, f : F) -> WithStmts<U> {
        WithStmts {
            val: f(self.val),
            stmts: self.stmts,
        }
    }
}

impl WithStmts<P<Expr>> {

    /// Package a series of statements and an expression into one block expression
    pub fn to_expr(mut self) -> P<Expr> {
        if self.stmts.is_empty() {
            self.val
        } else {
            self.stmts.push(mk().expr_stmt(self.val));
            mk().block_expr(mk().block(self.stmts))
        }
    }

    /// Package a series of statements and an expression into one block
    pub fn to_block(mut self) -> P<Block> {
        self.stmts.push(mk().expr_stmt(self.val));
        mk().block(self.stmts)
    }
}

fn pointer_offset(ptr: P<Expr>, offset: P<Expr>) -> P<Expr> {
    let offset = mk().cast_expr(offset, mk().path_ty(vec!["isize"]));
    mk().method_call_expr(ptr, "offset", vec![offset])
}

/// Construct a new constant null pointer expression
fn null_expr() -> P<Expr>  {
    mk().call_expr(mk().path_expr(vec!["std", "ptr", "null"]), vec![] as Vec<P<Expr>>)
}

/// Construct a new mutable null pointer expression
fn null_mut_expr() -> P<Expr> {
    mk().call_expr(mk().path_expr(vec!["std", "ptr", "null_mut"]), vec![] as Vec<P<Expr>>)
}

fn transmute_expr(source_ty: P<Ty>, target_ty: P<Ty>, expr: P<Expr>) -> P<Expr> {
    let type_args = vec![source_ty, target_ty];
    let path = vec![
        mk().path_segment("std"),
        mk().path_segment("mem"),
        mk().path_segment_with_params("transmute",
                                      mk().angle_bracketed_param_types(type_args)),
    ];
    mk().call_expr(mk().path_expr(path), vec![expr])
}

pub fn stmts_block(mut stmts: Vec<Stmt>) -> P<Block> {
    if stmts.len() == 1 {
        if let StmtKind::Expr(ref e) = stmts[0].node {
            if let ExprKind::Block(ref b) = e.node {
                    return b.clone()
            }
        }
    }

    if stmts.len() > 0 {
        let n = stmts.len() - 1;
        let s = stmts.remove(n);
        stmts.push(s.add_trailing_semicolon())
    }

    mk().block(stmts)
}

pub fn with_stmts_opt<T>(opt: Option<WithStmts<T>>) -> WithStmts<Option<T>> {
    match opt {
        None => WithStmts::new(None),
        Some(x) => WithStmts { stmts: x.stmts, val: Some(x.val) },
    }
}

// Generate link attributes needed to ensure that the generated Rust libraries have the right symbol
// values.
fn mk_linkage(in_extern_block: bool, new_name: &str, old_name: &str) -> Builder {
    if new_name == old_name {
        mk().single_attr("no_mangle")          // Don't touch my name Rust!
    } else if in_extern_block {
        mk().str_attr("link_name", old_name)   // Look for this name
    } else {
        mk().str_attr("export_name", old_name) // Make sure you actually name it this
    }
}


pub fn translate(ast_context: &TypedAstContext) -> String {

    let mut t = Translation::new(ast_context.clone());

    // Populate renamer with top-level names
    for top_id in &ast_context.c_decls_top {
        if let Some(y) = ast_context.index(*top_id).kind.get_name() {
            t.renamer.borrow_mut().insert(y.to_owned(), &y);
        }
    }

    for top_id in &ast_context.c_decls_top {
        let item = t.convert_decl(true, *top_id);
        t.items.push(item);
    }

    to_string(|s| {

        // Add `#![feature(libc)]` to the top of the file
        s.print_attribute(&mk().attribute::<_,TokenStream>(
            AttrStyle::Inner,
            vec!["feature"],
            vec![
                Token::OpenDelim(DelimToken::Paren),
                Token::Ident(mk().ident("libc")),
                Token::CloseDelim(DelimToken::Paren),
            ].into_iter().collect(),
        ))?;

        // Add `extern crate libc` to the top of the file
        s.print_item(&mk().extern_crate_item("libc", None))?;

        // Add the items accumulated
        for x in t.items.iter() {
            s.print_item(x)?;
        }

        Ok(())
    })
}

/// Convert a boolean expression to a c_int
fn bool_to_int(val: P<Expr>) -> P<Expr> {
    mk().cast_expr(val, mk().path_ty(vec!["libc","c_int"]))
}

/// This represents all of the ways a C expression can be used in a C program. Making this
/// distinction is important for:
///
///   * not generating a bunch of unnecessary code
///
///         Ex: The expression `p = 1` evaluates `1`, but when used in a statement like `p = 1;`, we
///             don't care about this, so we can translate to the Rust `p = 1` (even if it evaluates
///             to the unit type).
///
///             We get this behaviour by translating expression statements using `ExprUse::Unused`.
///
///   * handling `volatile` properly
///
///         Ex: Suppose `volatile int n, *p;` and `int x;`. Then, `x = n` is a volatile read of `n`
///             but `p = &n` is not.
///
///             We get this behaviour by translating the argument of `&` using `ExprUse::LValue` and
///             the right hand side of `=` using `ExprUse::RValue`.
///
/// See `Translation::convert_expr` for more details.
#[derive(Copy, Clone, Debug, PartialOrd, PartialEq, Ord, Eq)]
enum ExprUse {
    /// expressions interesting only for their side-effects - we don't care about their values
    Unused,
    /// expressions used as C lvalues
    LValue,
    /// expressions used as C rvalues
    RValue,
}

impl Translation {
    pub fn new(ast_context: TypedAstContext) -> Translation {
        Translation {
            items: vec![],
            type_converter: TypeConverter::new(),
            ast_context,
            renamer: RefCell::new(Renamer::new(vec![
                // Keywords currently in use
                "as", "break", "const", "continue", "crate","else", "enum", "extern", "false", "fn",
                "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
                "ref", "return", "Self", "self", "static", "struct", "super", "trait", "true",
                "type", "unsafe", "use", "where", "while",

                // Keywords reserved for future use
                "abstract", "alignof", "become", "box", "do", "final", "macro", "offsetof",
                "override", "priv", "proc", "pure", "sizeof", "typeof", "unsized", "virtual",
                "yield",
            ].iter().map(|s| s.to_string()).collect())),
            loops: RefCell::new(LoopContext::new()),
        }
    }

    // This node should _never_ show up in the final generated code. This is an easy way to notice
    // if it does.
    fn panic() -> P<Expr> {
        mk().mac_expr(mk().mac(vec!["panic"], vec![]))
    }

    fn convert_decl(&self, toplevel: bool, decl_id: CDeclId) -> P<Item> {
        match self.ast_context.index(decl_id).kind {
            CDeclKind::Struct{ref name, ref fields} => {
                // TODO: Add mapping from declaration ID to struct name to support unnamed structs
                if let &Some(ref name) = name {
                    let fields: Vec<StructField> = fields.into_iter().map(|x| {
                        let field_decl = self.ast_context.index(*x);
                        match &field_decl.kind {
                            &CDeclKind::Field {ref name, typ} => {
                                let typ = self.convert_type(typ.ctype);
                                mk().struct_field(name, typ)
                            }
                            _ => panic!("Found non-field in record field list"),
                        }
                    }).collect();

                    mk().pub_()
                        .call_attr("derive", vec!["Copy","Clone"])
                        .call_attr("repr", vec!["C"])
                        .struct_item(name, fields)
                } else {
                    panic!("Anonymous struct declarations not implemented")
                }
            }

            CDeclKind::Field { .. } => panic!("Field declarations should be handled inside structs/unions"),

            CDeclKind::Enum { name: None, .. } => panic!("Anonymous enums are not implemented"),
            CDeclKind::Enum { name: Some(ref name), ref variants } => {

                let enum_name = &self.renamer.borrow().get(name).expect("Enums should already be renamed");

                let variants: Vec<Variant> = variants
                    .into_iter()
                    .map(|v| {
                        let enum_constant_decl = self.ast_context.index(*v);
                        match &enum_constant_decl.kind {
                            &CDeclKind::EnumConstant { ref name, value } => {
                                let disc = mk().lit_expr(mk().int_lit(value as u128, ""));
                                let variant = &self.renamer.borrow_mut()
                                    .insert(name.to_owned(), &format!("{}::{}", enum_name, name))
                                    .expect(&format!("Failed to insert enum variant '{}'", name));
                                let variant = variant.trim_left_matches(&format!("{}::", enum_name));
                                mk().unit_variant(variant, Some(disc))
                            }
                            _ => panic!("Found non-variant in enum variant list"),
                        }
                    })
                    .collect();

                mk().pub_()
                    .call_attr("derive", vec!["Copy","Clone"])
                    .call_attr("repr", vec!["C"])
                    .enum_item(enum_name, variants)
            },

            CDeclKind::EnumConstant { .. } => panic!("Enum variants should be handled inside enums"),


            CDeclKind::Function { .. } if !toplevel => panic!("Function declarations must be top-level"),
            CDeclKind::Function { is_extern, typ, ref name, ref parameters, body } => {

                let new_name = &self.renamer.borrow().get(name).expect("Functions should already be renamed");

                let ret: CQualTypeId = match self.ast_context.index(typ).kind {
                    CTypeKind::Function(ret, _) => ret,
                    _ => panic!("Type of function {:?} was not a function type", decl_id)
                };

                let args: Vec<(String, CQualTypeId)> = parameters
                    .iter()
                    .map(|param_id  | {
                        if let CDeclKind::Variable { ref ident, typ, .. } = self.ast_context.index(*param_id).kind {
                            (ident.clone(), typ)
                        } else {
                            panic!("Parameter is not variable declaration")
                        }
                    })
                    .collect();

                self.convert_function(is_extern, new_name, name, &args, ret, body)
            },

            CDeclKind::Typedef { ref name, ref typ } => {

                let new_name = &self.renamer.borrow().get(name).expect("Typedefs should already be renamed");

                let ty = self.convert_type(typ.ctype);
                mk().type_item(new_name, ty)
            },

            // Extern variable without intializer (definition elsewhere)
            CDeclKind::Variable { is_extern: true, is_static, ref ident, initializer: None, typ } => {
                assert!(is_static, "An extern variable must be static");

                let new_name = &self.renamer.borrow().get(ident).expect("Variables should already be renamed");
                let (ty, mutbl, _) = self.convert_variable(None, typ);

                let extern_item = mk_linkage(true, new_name, ident)
                    .set_mutbl(mutbl)
                    .foreign_static(new_name, ty);

                mk().abi(Abi::C)
                    .foreign_items(vec![extern_item])
            }

            // Extern variable with intializer (definition here)
            CDeclKind::Variable { is_extern: true, is_static, ref ident, initializer, typ } => {
                assert!(is_static, "An extern variable must be static");

                let new_name = &self.renamer.borrow().get(ident).expect("Variables should already be renamed");
                let (ty, mutbl, init) = self.convert_variable(initializer, typ);

                let init = init.to_expr();

                mk_linkage(false, new_name, ident)
                    .vis(Visibility::Public)
                    .abi(Abi::C)
                    .set_mutbl(mutbl)
                    .static_item(new_name, ty, init)
            }

            // Static variable (definition here)
            CDeclKind::Variable { is_static: true, ref ident, initializer, typ, .. } => {

                let new_name = &self.renamer.borrow().get(ident).expect("Variables should already be renamed");
                let (ty, mutbl, init) = self.convert_variable(initializer, typ);

                let init = init.to_expr();

                mk().set_mutbl(mutbl)
                    .static_item(new_name, ty, init)
            }

            CDeclKind::Variable { .. } => panic!("This should be handled in 'convert_decl_stmt'"),

            _ => unimplemented!()
        }
    }

    fn convert_function(
        &self,
        is_extern: bool,
        new_name: &str,
        name: &str,
        arguments: &[(String, CQualTypeId)],
        return_type: CQualTypeId,
        body: Option<CStmtId>,
    ) -> P<Item> {

        // Start scope for function parameters
        self.renamer.borrow_mut().add_scope();

        let args: Vec<Arg> = arguments
            .iter()
            .map(|&(ref var, typ)| {
                let rust_var = self.renamer.borrow_mut()
                .insert(var.to_string(), var.as_str())
                .expect(&format!("Failed to insert argument '{}'", var));

                let (ty, mutbl, _) = self.convert_variable(None, typ);

                let pat = mk().set_mutbl(mutbl).ident_pat(rust_var);

                mk().arg(ty, pat)
            })
            .collect();

        let ret = FunctionRetTy::Ty(self.convert_type(return_type.ctype));

        let decl = mk().fn_decl(args, ret);

        let item = if let Some(body) = body {
            // Translating an actual function

            let block = self.convert_function_body(body);

            // Only add linkage attributes if the function is `extern`
            let mk_ = if is_extern {
                mk_linkage(false, new_name, name)
                    .abi(Abi::C)
                    .vis(Visibility::Public)
            } else {
                mk().abi(Abi::C)
            };

            mk_.unsafe_().fn_item(new_name, decl, block)

        } else {
            // Translating an extern function declaration

            let function_decl = mk_linkage(true, new_name, name)
                .foreign_fn(new_name, decl);

            mk().abi(Abi::C)
                .foreign_items(vec![function_decl])
        };

        // End scope for function parameters
        self.renamer.borrow_mut().drop_scope();

        item
    }

    fn convert_function_body(&self, body_id: CStmtId) -> P<Block> {

        // Open function body scope
        self.renamer.borrow_mut().add_scope();

        let stmts = match self.ast_context.index(body_id).kind {
            CStmtKind::Compound(ref stmts) => stmts
                .iter()
                .flat_map(|stmt| self.convert_stmt(*stmt))
                .collect(),
            _ => panic!("function body expects to be a compound statement"),
        };

        // Close function body scope
        self.renamer.borrow_mut().drop_scope();

        stmts_block(stmts)
    }

    fn convert_stmt(&self, stmt_id: CStmtId) -> Vec<Stmt> {
        match self.ast_context.index(stmt_id).kind {
            CStmtKind::Empty => vec![],

            CStmtKind::Decls(ref decls) => {
                decls
                    .iter()
                    .flat_map(|decl| self.convert_decl_stmt(*decl))
                    .collect()
            },

            CStmtKind::Return(expr) => self.convert_return_stmt(expr),

            CStmtKind::If { scrutinee, true_variant, false_variant } =>
                self.convert_if_stmt(scrutinee, true_variant, false_variant),

            CStmtKind::While { condition, body } => self.convert_while_stmt(condition, body),

            CStmtKind::DoWhile { body, condition } => self.convert_do_stmt(body, condition),

            CStmtKind::ForLoop { init, condition, increment, body } =>
                self.convert_for_stmt(init, condition, increment, body),

            CStmtKind::Compound(ref stmts) => {
                self.renamer.borrow_mut().add_scope();

                let stmts = stmts
                    .iter()
                    .flat_map(|stmt| self.convert_stmt(*stmt))
                    .collect();

                self.renamer.borrow_mut().drop_scope();

                vec![mk().expr_stmt(mk().block_expr(stmts_block(stmts)))]
            },

            CStmtKind::Expr(expr) => self.convert_expr(ExprUse::Unused, expr).stmts,

            CStmtKind::Break => {
                let mut loops = self.loops.borrow_mut();
                loops.current_loop_mut().has_break = true;
                let loop_label = loops.current_loop_label();
                vec![mk().expr_stmt(mk().break_expr(Some(loop_label)))]
            },

            ref stmt => unimplemented!("convert_stmt {:?}", stmt),
        }
    }

    /// Convert a C expression to a rust boolean expression
    fn convert_condition(&self, target: bool, cond_id: CExprId) -> WithStmts<P<Expr>> {
        let ty_id = self.ast_context.index(cond_id).kind.get_type();

        self.convert_expr(ExprUse::RValue, cond_id)
            .map(|e| self.match_bool(target, ty_id, e))
    }

    fn convert_while_stmt(&self, cond_id: CExprId, body_id: CStmtId) -> Vec<Stmt> {
        let cond = self.convert_condition(true, cond_id);
        self.loops.borrow_mut().push_loop(LoopType::While);
        let body = self.convert_stmt(body_id);
        let loop_ = self.loops.borrow_mut().pop_loop();

        let rust_cond = cond.to_expr();
        let rust_body = stmts_block(body);

        vec![mk().expr_stmt(mk().while_expr(rust_cond, rust_body, loop_.label))]
    }

    fn convert_do_stmt(&self, body_id: CStmtId, cond_id: CExprId) -> Vec<Stmt> {
        let cond = self.convert_condition(false, cond_id);
        self.loops.borrow_mut().push_loop(LoopType::DoWhile);
        let mut body = self.convert_stmt(body_id);
        let mut loop_label = self.loops.borrow_mut().current_loop_label();
        let mut loop_ = self.loops.borrow_mut().pop_loop();

        let rust_cond = cond.to_expr();
        let break_stmt = mk().semi_stmt(mk().break_expr(Some(loop_label)));

        body.push(mk().expr_stmt(mk().ifte_expr(rust_cond, mk().block(vec![break_stmt]), None as Option<P<Expr>>)));

        let rust_body = stmts_block(body);

        vec![mk().semi_stmt(mk().loop_expr(rust_body, loop_.label))]
    }

    fn convert_for_stmt(
        &self,
        init_id: Option<CStmtId>,
        cond_id: Option<CExprId>,
        inc_id: Option<CExprId>,
        body_id: CStmtId,
    ) -> Vec<Stmt> {

        self.renamer.borrow_mut().add_scope();

        let mut init = match init_id {
          Some(i) => self.convert_stmt(i),
          None => vec![],
        };

        let mut inc = match inc_id {
            Some(i) => self.convert_expr(ExprUse::Unused, i).stmts,
            None => vec![],
        };

        self.loops.borrow_mut().push_loop(LoopType::For);
        let mut body = self.convert_stmt(body_id);
        let loop_ = self.loops.borrow_mut().pop_loop();
        body.append(&mut inc);

        let body_block = stmts_block(body);

        let looper = match cond_id {
            None => mk().loop_expr(body_block, loop_.label), // loop
            Some(i) => mk().while_expr(self.convert_condition(true, i).to_expr(), body_block, loop_.label), // while
        };

        init.push(mk().expr_stmt(looper));

        self.renamer.borrow_mut().drop_scope();

        vec![mk().expr_stmt(mk().block_expr(mk().block(init)))]

    }

    fn convert_if_stmt(
        &self,
        cond_id: CExprId,
        then_id: CStmtId,
        else_id: Option<CStmtId>
    ) -> Vec<Stmt> {
        let mut cond = self.convert_condition(true, cond_id);
        let then_stmts = stmts_block(self.convert_stmt(then_id));
        let else_stmts = else_id.map(|x| { mk().block_expr(stmts_block(self.convert_stmt(x)))});

        cond.stmts.push(mk().semi_stmt(mk().ifte_expr(cond.val, then_stmts, else_stmts)));
        cond.stmts
    }

    fn convert_return_stmt(&self, result_id: Option<CExprId>) -> Vec<Stmt> {
        let val = result_id.map(|i| self.convert_expr(ExprUse::RValue, i));
        let mut ws = with_stmts_opt(val);
        let ret = mk().expr_stmt(mk().return_expr(ws.val));

        ws.stmts.push(ret);
        ws.stmts
    }

    fn convert_decl_stmt(&self, decl_id: CDeclId) -> Vec<Stmt> {

        match self.ast_context.index(decl_id).kind {

            CDeclKind::Variable { is_static, is_extern, ref ident, initializer, typ } if !is_static && !is_extern => {

                let rust_name = self.renamer.borrow_mut()
                    .insert(ident.clone(), &ident)
                    .expect(&format!("Failed to insert variable '{}'", ident));
                let (ty, mutbl, init) = self.convert_variable(initializer, typ);

                let pat = mk().set_mutbl(mutbl).ident_pat(rust_name);
                let local = mk().local(pat, Some(ty), Some(init.val));

                let mut stmts = init.stmts;
                stmts.push(mk().local_stmt(P(local)));
                stmts
            }

            ref decl => {

                let inserted = if let Some(ident) = decl.get_name() {
                    self.renamer.borrow_mut()
                        .insert(ident.clone(), &ident)
                        .is_some()
                } else {
                    false
                };

                // TODO: We need this because we can have multiple 'extern' decls of the same variable.
                //       When we do, we must make sure to insert into the renamer the first time, and
                //       then skip subsequent times.
                let skip = match decl {
                    &CDeclKind::Variable { .. } => !inserted,
                    _ => false,
                };

                if skip {
                    vec![]
                } else {
                    let item = self.convert_decl(false, decl_id);
                    vec![mk().item_stmt(item)]
                }
            },
        }
    }

    fn convert_variable(
        &self,
        initializer: Option<CExprId>,
        typ: CQualTypeId
    ) -> (P<Ty>, Mutability, WithStmts<P<Expr>>) {
        let init = match initializer {
            Some(x) => self.convert_expr(ExprUse::RValue, x),
            None => WithStmts::new(self.implicit_default_expr(typ.ctype)),
        };
        let ty = self.convert_type(typ.ctype);
        let mutbl = if typ.qualifiers.is_const { Mutability::Immutable } else { Mutability:: Mutable };

        (ty, mutbl, init)
    }

    fn convert_type(&self, type_id: CTypeId) -> P<Ty> {
        self.type_converter.convert(&self.ast_context, &self.renamer.borrow(), type_id)
    }

    /// Write to a `lhs` that is volatile
    pub fn volatile_write(&self, lhs: &P<Expr>, lhs_type: CTypeId, rhs: P<Expr>) -> P<Expr> {

        let addr_lhs = match lhs.node {
            ExprKind::Unary(ast::UnOp::Deref, ref e) => e.clone(),
            _ => {
                let addr_lhs = mk().mutbl().addr_of_expr(lhs);

                let lhs_type = self.convert_type(lhs_type);
                let ty = mk().mutbl().ptr_ty(lhs_type);

                mk().cast_expr(addr_lhs, ty)
            },
        };

        mk().call_expr(mk().path_expr(vec!["std","ptr","write_volatile"]), vec![addr_lhs, rhs])
    }

    /// Read from a `lhs` that is volatile
    pub fn volatile_read(&self, lhs: &P<Expr>, lhs_type: CTypeId) -> P<Expr> {

        let addr_lhs = match lhs.node {
            ExprKind::Unary(ast::UnOp::Deref, ref e) => e.clone(),
            _ => {
                let addr_lhs = mk().addr_of_expr(lhs);

                let lhs_type = self.convert_type(lhs_type);
                let ty = mk().ptr_ty(lhs_type);

                mk().cast_expr(addr_lhs, ty)
            }
        };

        mk().call_expr(mk().path_expr(vec!["std","ptr","read_volatile"]), vec![addr_lhs])
    }

    /// Translate a C expression into a Rust one, possibly collecting side-effecting statements
    /// to run before the expression.
    ///
    /// The `use_` argument informs us how the C expression we are translating is used in the C
    /// program. See `ExprUse` for more information.
    ///
    /// In the case that `use_` is `ExprUse::Unused`, all side-effecting components will be in the
    /// `stmts` field of the output and it is expected that the `val` field of the output will be
    /// ignored.
    fn convert_expr(&self, use_: ExprUse, expr_id: CExprId) -> WithStmts<P<Expr>> {

        match self.ast_context.index(expr_id).kind {

            CExprKind::UnaryType(_ty, kind, arg_ty) => {
                let ty = self.convert_type(arg_ty.ctype);
                let name = match kind {
                    UnTypeOp::SizeOf => "size_of",
                    UnTypeOp::AlignOf => "align_of",
                };
                let tys = vec![ty];
                let path = vec![mk().path_segment("std"),
                                mk().path_segment("mem"),
                                mk().path_segment_with_params(name,
                                mk().angle_bracketed_param_types(tys)),
                ];
                let call = mk().call_expr(mk().path_expr(path), vec![] as Vec<P<Expr>>);
                let casted = mk().cast_expr(call, mk().path_ty(vec!["libc","size_t"]));
                WithStmts::new(casted)
            }

            CExprKind::DeclRef(qual_ty, decl_id) => {
                let decl = &self.ast_context.index(decl_id).kind;
                let varname = decl.get_name().expect("expected variable name").to_owned();
                let rustname = self.renamer.borrow_mut()
                    .get(&varname)
                    .expect(&format!("name not declared: '{}'", varname));

                let mut val = mk().path_expr(vec![rustname]);

                // If the variable is volatile and used as something that isn't an LValue, this
                // constitutes a volatile read.
                if use_ != ExprUse::LValue && qual_ty.qualifiers.is_volatile {
                    val = self.volatile_read(&val, qual_ty.ctype);
                }

                // If the variable is actually an `EnumConstant`, we need to add a cast to the
                // expected integral type. When modifying this, look at `Translation::enum_cast` -
                // this function assumes `DeclRef`'s to `EnumConstants`'s will translate to casts.
                if let &CDeclKind::EnumConstant { .. } = decl {
                    let ty = self.convert_type(qual_ty.ctype);
                    val = mk().cast_expr(val, ty);
                }

                WithStmts::new(val)
            }

            CExprKind::Literal(_, CLiteral::Integer(val)) => {
                WithStmts::new(mk().lit_expr(mk().int_lit(val.into(), LitIntType::Unsuffixed)))
            }

            CExprKind::Literal(_, CLiteral::Character(val)) => {
                WithStmts::new(mk().lit_expr(mk().int_lit(val.into(), LitIntType::Unsuffixed)))
            }

            CExprKind::Literal(ty, CLiteral::Floating(val)) => {

                let mut bytes: Vec<u8> = vec![];
                dtoa::write(&mut bytes, val);
                let str = String::from_utf8(bytes).unwrap();
                let float_ty = match &self.ast_context.resolve_type(ty.ctype).kind {
                    &CTypeKind::Double => FloatTy::F64,
                    &CTypeKind::Float => FloatTy::F32,
                    k => panic!("Unsupported floating point literal type {:?}", k),
                };
                WithStmts::new(mk().lit_expr(mk().float_lit(str, float_ty)))
            }

            CExprKind::Literal(ty, CLiteral::String(ref val, width)) => {
                let mut val = val.to_owned();

                // Add zero terminator
                for _ in 0..width { val.push(0); }

                let u8_ty = mk().path_ty(vec!["u8"]);
                let width_lit = mk().lit_expr(mk().int_lit(val.len() as u128, LitIntType::Unsuffixed));
                let array_ty = mk().array_ty(u8_ty, width_lit);
                let source_ty = mk().ref_ty(array_ty);
                let target_ty = mk().ref_ty(self.convert_type(ty.ctype));

                let byte_literal = mk().lit_expr(mk().bytestr_lit(val));
                let pointer = transmute_expr(source_ty, target_ty, byte_literal);
                WithStmts::new(pointer)
            }

            CExprKind::ImplicitCast(ty, expr, kind) | CExprKind::ExplicitCast(ty, expr, kind) => {
                let val = self.convert_expr(use_, expr);

                match kind {
                    CastKind::BitCast => {
                        val.map(|x| {
                            // TODO: Detect cast from mutable to constant pointer to same type
                            let source_ty_id = self.ast_context.index(expr).kind.get_type();
                            let source_ty = self.convert_type(source_ty_id);
                            let target_ty = self.convert_type(ty.ctype);
                            transmute_expr(source_ty, target_ty, x)
                        })
                    }

                    CastKind::IntegralToPointer | CastKind::PointerToIntegral |
                    CastKind::IntegralCast | CastKind::FloatingCast | CastKind::FloatingToIntegral | CastKind::IntegralToFloating => {

                        let target_ty = self.convert_type(ty.ctype);
                        let target_ty_ctype = &self.ast_context.resolve_type(ty.ctype).kind;

                        let source_ty_ctype_id = self.ast_context.index(expr).kind.get_type();

                        if let &CTypeKind::Enum(enum_decl_id) = target_ty_ctype {
                            // Casts targeting `enum` types...
                            let source_ty = self.convert_type(source_ty_ctype_id);
                            self.enum_cast(enum_decl_id, expr, val, source_ty, target_ty)
                        } else {
                            // Other numeric casts translate to Rust `as` casts

                            val.map(|x| mk().cast_expr(x, target_ty))
                        }
                    }

                    CastKind::LValueToRValue | CastKind::NoOp | CastKind::ToVoid => val,

                    CastKind::FunctionToPointerDecay =>
                        val.map (|x| mk().call_expr(mk().ident_expr("Some"), vec![x])),

                    CastKind::ArrayToPointerDecay =>
                        val.map(|x| mk().method_call_expr(x, "as_mut_ptr", vec![] as Vec<P<Expr>>)),

                    CastKind::NullToPointer => {
                        assert!(val.stmts.is_empty());

                        let res = if self.is_function_pointer(ty.ctype) {
                            let source_ty = mk().ptr_ty(mk().path_ty(vec!["libc","c_void"]));
                            let target_ty = self.convert_type(ty.ctype);
                            transmute_expr(source_ty, target_ty, null_expr())
                        } else {
                            match &self.ast_context.resolve_type(ty.ctype).kind {
                                &CTypeKind::Pointer(pointee) if pointee.qualifiers.is_const => null_expr(),
                                _ => null_mut_expr(),
                            }
                        };

                        WithStmts::new(res)
                    }

                    CastKind::ToUnion => panic!("TODO cast to union not supported"),

                    CastKind::IntegralToBoolean | CastKind::FloatingToBoolean => {
                        let val_ty = self.ast_context.index(expr).kind.get_type();
                        val.map(|x| self.match_bool(true, val_ty, x))
                    }

                    // I don't know how to actually cause clang to generate this
                    CastKind::BooleanToSignedIntegral =>
                        panic!("TODO boolean to signed integral not supported"),

                    CastKind::FloatingRealToComplex | CastKind::FloatingComplexToIntegralComplex |
                    CastKind::FloatingComplexCast | CastKind::FloatingComplexToReal |
                    CastKind::IntegralComplexToReal | CastKind::IntegralRealToComplex |
                    CastKind::IntegralComplexCast | CastKind:: IntegralComplexToFloatingComplex |
                    CastKind::IntegralComplexToBoolean =>
                        panic!("TODO casts with complex numbers not supported"),
                }
            }

            CExprKind::Unary(type_id, op, arg) =>
                self.convert_unary_operator(use_,op, type_id, arg),

            CExprKind::Conditional(_, cond, lhs, rhs) => {
                let cond = self.convert_condition(true, cond);

                let lhs = self.convert_expr(use_, lhs);
                let rhs = self.convert_expr(use_, rhs);

                if use_ == ExprUse::Unused {
                    let then: P<Block> = mk().block(lhs.stmts);
                    let els: P<Expr> = mk().block_expr(mk().block(rhs.stmts));

                    cond.and_then(|c| WithStmts {
                        stmts: vec![mk().semi_stmt(mk().ifte_expr(c, then, Some(els)))],
                        val: Translation::panic(),
                    })
                } else {
                    let then: P<Block> = lhs.to_block();
                    let els: P<Expr> = rhs.to_expr();

                    cond.map(|c| mk().ifte_expr(c, then, Some(els)))
                }
            },

            CExprKind::Binary(ref type_id, ref op, lhs, rhs) => {

                match *op {
                    c_ast::BinOp::Comma => {

                        // The value of the LHS of a comma expression is always discarded
                        let lhs = self.convert_expr(ExprUse::Unused, lhs);
                        let rhs = self.convert_expr(use_, rhs);

                        WithStmts {
                            stmts: lhs.stmts.into_iter().chain(rhs.stmts).collect(),
                            val: rhs.val,
                        }
                    }

                    c_ast::BinOp::And => {
                        // XXX: do we need the RHS to always be used?
                        let lhs_ty = self.ast_context.index(lhs).kind.get_type();
                        let rhs_ty = self.ast_context.index(rhs).kind.get_type();

                        let lhs =
                            self.convert_expr(ExprUse::RValue, lhs)
                                .map(|x| self.match_bool(true, lhs_ty, x));
                        let rhs =
                            self.convert_expr(ExprUse::RValue, rhs)
                                .map(|x| self.match_bool(true, rhs_ty, x));

                        lhs.map(|x| bool_to_int(mk().binary_expr(BinOpKind::And, x, rhs.to_expr())))
                    }

                    c_ast::BinOp::Or => {
                        // XXX: do we need the RHS to always be used?
                        let lhs_ty = self.ast_context.index(lhs).kind.get_type();
                        let rhs_ty = self.ast_context.index(rhs).kind.get_type();

                        let lhs =
                            self.convert_expr(ExprUse::RValue, lhs)
                                .map(|x| self.match_bool(true, lhs_ty, x));
                        let rhs =
                            self.convert_expr(ExprUse::RValue, rhs)
                                .map(|x| self.match_bool(true, rhs_ty, x));

                        lhs.map(|x| bool_to_int(mk().binary_expr(BinOpKind::Or, x, rhs.to_expr())))
                    }

                    // No sequence-point cases

                    c_ast::BinOp::AssignAdd |
                    c_ast::BinOp::AssignSubtract |
                    c_ast::BinOp::AssignMultiply |
                    c_ast::BinOp::AssignDivide |
                    c_ast::BinOp::AssignModulus |
                    c_ast::BinOp::AssignBitXor |
                    c_ast::BinOp::AssignShiftLeft |
                    c_ast::BinOp::AssignShiftRight |
                    c_ast::BinOp::AssignBitOr |
                    c_ast::BinOp::AssignBitAnd |
                    c_ast::BinOp::Assign => {
                        let ty = self.convert_type(type_id.ctype);

                        self.convert_assignment_operator(use_, *op, ty, type_id.ctype, lhs, rhs)
                    },

                    _ => {
                        let ty = self.convert_type(type_id.ctype);

                        let lhs_type = self.ast_context.index(lhs).kind.get_qual_type();
                        let rhs_type = self.ast_context.index(rhs).kind.get_qual_type();

                        let WithStmts { val: lhs, stmts: lhs_stmts } = self.convert_expr(ExprUse::RValue, lhs);
                        let WithStmts { val: rhs, stmts: rhs_stmts } = self.convert_expr(ExprUse::RValue, rhs);

                        let mut stmts = vec![];
                        stmts.extend(lhs_stmts);
                        stmts.extend(rhs_stmts);

                        let val = self.convert_binary_operator(*op, ty, type_id.ctype, lhs_type, rhs_type, lhs, rhs);

                        WithStmts { stmts, val }
                    }
                }
            }

            CExprKind::ArraySubscript(_, ref lhs, ref rhs) => {
                let lhs_node = &self.ast_context.index(*lhs).kind;
                let lhs_is_pointer = self.ast_context.resolve_type(lhs_node.get_type()).kind.is_pointer();

                // From here on in, the LHS is the pointer/array and the RHS the index
                let (lhs,rhs) = if lhs_is_pointer { (lhs, rhs) } else { (rhs, lhs) };

                let mut stmts = vec![];

                let rhs = self.convert_expr(ExprUse::RValue, *rhs);
                stmts.extend(rhs.stmts);

                let val = if let &CExprKind::ImplicitCast(_, ref arr, CastKind::ArrayToPointerDecay) = lhs_node {
                    // If the LHS just underwent an implicit cast from array to pointer, bypass that
                    // to make an actual Rust indexing operation

                    let lhs = self.convert_expr(use_, *arr);
                    stmts.extend(lhs.stmts);

                    mk().index_expr(lhs.val, rhs.val)
                } else {
                    // Otherwise, use the pointer and make a deref of a pointer offset expression

                    let lhs = self.convert_expr(ExprUse::RValue, *lhs);
                    stmts.extend(lhs.stmts);

                    mk().unary_expr(ast::UnOp::Deref, pointer_offset(lhs.val, rhs.val))
                };

                WithStmts { stmts, val }
            }

            CExprKind::Call(_, func, ref args) => {

                let WithStmts { mut stmts, val: func } = match self.ast_context.index(func).kind {
                    CExprKind::ImplicitCast(_, fexp, CastKind::FunctionToPointerDecay) =>
                        self.convert_expr(ExprUse::RValue, fexp),
                    _ => {
                        self.convert_expr(ExprUse::RValue, func).map(|x|
                        mk().method_call_expr(x, "unwrap", vec![] as Vec<P<Expr>>))
                    }
                };

                let mut args_new: Vec<P<Expr>> = vec![];
                for arg in args {
                    let WithStmts { stmts: ss, val } = self.convert_expr(ExprUse::RValue, *arg);
                    stmts.extend(ss);
                    args_new.push(val);
                }

                let call_expr = mk().call_expr(func, args_new);

                if use_ == ExprUse::Unused {
                    // Recall that if `used` is false, the `stmts` field of the output must contain
                    // all side-effects (and a function call can always have side-effects)
                    stmts.push(mk().semi_stmt(call_expr));

                    WithStmts { stmts, val: Translation::panic() }
                } else {
                    WithStmts { stmts, val: call_expr }
                }
            }

            CExprKind::Member(_, expr, decl, kind) => {
                let struct_val = self.convert_expr(use_, expr);
                let field_name = self.ast_context.index(decl).kind.get_name().expect("expected field name");

                if use_ == ExprUse::Unused {
                    struct_val
                } else {
                    struct_val.map(|v| {
                        let v = match kind {
                            MemberKind::Arrow => mk().unary_expr(ast::UnOp::Deref, v),
                            MemberKind::Dot => v,
                        };
                        mk().field_expr(v, field_name)
                    })
                }
            }

            CExprKind::CompoundLiteral(_, val) =>
                self.convert_expr(use_, val),

            CExprKind::InitList(ty, ref ids) => {
                let resolved = &self.ast_context.resolve_type(ty.ctype).kind;

                match resolved {
                    &CTypeKind::ConstantArray(ty, n) => {
                        // Convert all of the provided initializer values
                        let mut stmts: Vec<Stmt> = vec![];
                        let mut vals: Vec<P<Expr>> = vec![];
                        for v in ids {
                            let mut x = self.convert_expr(ExprUse::RValue, *v);
                            stmts.append(&mut x.stmts);
                            vals.push(x.val);
                        }


                        // Pad out the array literal with default values to the desired size
                        for _i in ids.len()..n {
                            vals.push(self.implicit_default_expr(ty))
                        }

                        WithStmts {
                            stmts,
                            val: mk().array_expr(vals),
                        }
                    }
                    &CTypeKind::Struct(struct_id) => {
                        self.convert_struct_literal(struct_id,  ids.as_ref(), ty)
                    }
                    t => {
                        panic!("Init list not implemented for {:?}", t);
                    }
                }
            }
            CExprKind::ImplicitValueInit(ty) =>
                WithStmts::new(self.implicit_default_expr(ty.ctype)),
        }
    }

    /// This handles translating casts when the target type in an `enum` type.
    ///
    /// When translating variable references to `EnumConstant`'s, we always insert casts to the
    /// expected type. In C, `EnumConstants` have some integral type, _not_ the enum type. However,
    /// if we then immediately have a cast to convert this variable back into an enum type, we would
    /// like to produce Rust with _no_ casts. This function handles this simplification.
    fn enum_cast(
        &self,
        enum_decl: CEnumId,      // ID of the enum declaration corresponding to the target type
        expr: CExprId,           // ID of initial C argument to cast
        val: WithStmts<P<Expr>>, // translated Rust argument to cast
        source_ty: P<Ty>,        // source type of cast
        target_ty: P<Ty>,        // target type of cast
    ) -> WithStmts<P<Expr>> {

        // Extract the IDs of the `EnumConstant` decls underlying the enum.
        let variants = match &self.ast_context.index(enum_decl).kind {
            &CDeclKind::Enum { ref variants, .. } => variants,
            _ => panic!("{:?} does not point to an `enum` declaration")
        };

        match &self.ast_context.index(expr).kind {
            // This is the case of finding a variable which is an `EnumConstant` of the same enum
            // we are casting to. Here, we can just remove the extraneous cast instead of generating
            // a new one.
            &CExprKind::DeclRef(_, decl_id) if variants.contains(&decl_id) =>
                val.map(|x| match x.node {
                    ast::ExprKind::Cast(ref e, _) => e.clone(),
                    _ => panic!(format!("DeclRef {:?} of enum {:?} is not cast", expr, enum_decl)),
                }),

            // In all other cases, a cast to an enum requires a `transmute` - Rust enums cannot be
            // converted into integral types as easily as C ones.
            _ => val.map(|x| transmute_expr(source_ty, target_ty, x)),
        }

    }

    fn convert_struct_literal(&self, struct_id: CRecordId, ids: &[CExprId], ty: CQualTypeId) -> WithStmts<P<Expr>> {

        let struct_decl = &self.ast_context.index(struct_id).kind;

        let (struct_name, field_decls) = match struct_decl {
            &CDeclKind::Struct { ref name, ref fields } => {
                let fieldnames: Vec<(String, CQualTypeId)> = fields.iter().map(|x| {
                    if let &CDeclKind::Field { ref name, typ } = &self.ast_context.index(*x).kind {
                        (name.to_owned(), typ)
                    } else {
                        panic!("Struct field decl type mismatch")
                    }
                }).collect();

                (name.to_owned().unwrap(), fieldnames)
            }
            _ => panic!("Struct literal declaration mismatch"),
        };

        let mut stmts: Vec<Stmt> = vec![];
        let mut fields: Vec<Field> = vec![];

        // Add specified record fields
        for i in 0usize..ids.len() {
            let v = ids[i];
            let &(ref field_name, _) = &field_decls[i];

            let mut x = self.convert_expr(ExprUse::RValue, v);
            stmts.append(&mut x.stmts);
            fields.push(mk().field(field_name, x.val));
        }

        // Pad out remaining omitted record fields
        for i in ids.len()..fields.len() {
            let &(ref field_name, ty) = &field_decls[i];
            fields.push(mk().field(field_name, self.implicit_default_expr(ty.ctype)));
        }

        WithStmts {
            stmts,
            val: mk().struct_expr(vec![mk().path_segment(struct_name)], fields)
        }
    }

    pub fn implicit_default_expr(&self, ty_id: CTypeId) -> P<Expr> {
        let resolved_ty = &self.ast_context.resolve_type(ty_id).kind;

        if resolved_ty.is_integral_type() {
            mk().lit_expr(mk().int_lit(0, LitIntType::Unsuffixed))
        } else if resolved_ty.is_floating_type() {
            mk().lit_expr(mk().float_unsuffixed_lit("0."))
        } else if self.is_function_pointer(ty_id) {
            let source_ty = mk().ptr_ty(mk().path_ty(vec!["libc","c_void"]));
            let target_ty = self.convert_type(ty_id);
            transmute_expr(source_ty, target_ty, null_expr())
        } else if let &CTypeKind::Pointer(p) = resolved_ty {
            if p.qualifiers.is_const { null_expr() } else { null_mut_expr() }
        } else if let &CTypeKind::ConstantArray(elt, sz) = resolved_ty {
            let sz = mk().lit_expr(mk().int_lit(sz as u128, LitIntType::Unsuffixed));
            mk().repeat_expr(self.implicit_default_expr(elt), sz)
        } else {
            mk().call_expr(mk().path_expr(vec!["Default", "default"]), vec![] as Vec<P<Expr>>)
        }
    }

    /// Get back a Rust lvalue corresponding to the expression passed in.
    ///
    /// Do not use the output lvalue expression more than once.
    pub fn name_reference_write(
        &self,
        reference: CExprId,
    ) -> WithStmts<P<Expr>> {
        self.name_reference(reference, false)
            .map(|(lvalue, _)| lvalue)
    }

    /// Get back a Rust (lvalue, rvalue) pair corresponding to the expression passed in.
    ///
    /// You may reuse either of these expressions.
    pub fn name_reference_write_read(
        &self,
        reference: CExprId,
    ) -> WithStmts<(P<Expr>, P<Expr>)> {
        let msg: &str = "When called with `uses_read = true`, `name_reference` should always \
                       return an rvalue (something from which to read the memory location)";

        self.name_reference(reference, true)
            .map(|(lvalue, rvalue)| (lvalue, rvalue.expect(msg)))
    }

    /// This function transforms an expression that should refer to a memory location (a C lvalue)
    /// into a Rust lvalue for writing to that location.
    ///
    /// When called with `uses_read`, this function returns an rvalue too. The rvalue can be used to
    /// read multiple times without duplicating side-effects.
    ///
    /// NOTE: Use `name_reference_write` or `name_reference_write_read` instead of calling this
    ///       directly.
    fn name_reference(
        &self,
        reference: CExprId,
        uses_read: bool,
    ) -> WithStmts<(P<Expr>, Option<P<Expr>>)> {

        let reference_ty = self.ast_context.index(reference).kind.get_qual_type();
        let WithStmts {
            val: reference,
            mut stmts,
        } = self.convert_expr(ExprUse::LValue, reference);

        /// Check if something is a valid Rust lvalue. Inspired by `librustc::ty::expr_is_lval`.
        fn is_lvalue(e: &Expr) -> bool {
            match e.node {
                ExprKind::Path(..) |
                ExprKind::Unary(ast::UnOp::Deref, _) |
                ExprKind::Field(..) |
                ExprKind::TupField(..) |
                ExprKind::Index(..) => true,
                _ => false,
            }
        }

        // Check if something is a side-effect free Rust lvalue.
        fn is_simple_lvalue(e: &Expr) -> bool {
            match e.node {
                ExprKind::Path(..) => true,
                ExprKind::Unary(ast::UnOp::Deref, ref e)  |
                ExprKind::Field(ref e, _) |
                ExprKind::TupField(ref e, _) |
                ExprKind::Index(ref e, _) => is_simple_lvalue(e),
                _ => false,
            }
        }

        // Given the LHS access to a variable, produce the RHS one
        let read = |write: P<Expr>| -> P<Expr> {
            if reference_ty.qualifiers.is_volatile {
                self.volatile_read(&write, reference_ty.ctype)
            } else {
                write
            }
        };

        if !uses_read && is_lvalue(&*reference) {
            WithStmts { stmts, val: (reference, None) }
        } else if is_simple_lvalue(&*reference) {
            WithStmts { stmts, val:(reference.clone(), Some(read(reference))) }
        } else {
            // This is the case where we explicitly need to factor out possible side-effects.

            let ptr_name = self.renamer.borrow_mut().fresh();

            // let ref mut p = lhs;
            let compute_ref =
                mk().local_stmt(
                    P(mk().local(mk().mutbl().ident_ref_pat(&ptr_name),
                                 None as Option<P<Ty>>,
                                 Some(reference)))
                );
            stmts.push(compute_ref);

            let write = mk().unary_expr(ast::UnOp::Deref, mk().ident_expr(&ptr_name));

            WithStmts {
                stmts,
                val: (write.clone(), Some(read(write))),
            }
        }
    }

    pub fn convert_pre_increment(&self, ty: CQualTypeId, up: bool, arg: CExprId) -> WithStmts<P<Expr>> {

        let WithStmts{ val: (write, read), stmts: mut lhs_stmts } = self.name_reference_write_read(arg);

        let one = mk().lit_expr(mk().int_lit(1, LitIntType::Unsuffixed));
        // *p + 1
        let val =
            if self.ast_context.resolve_type(ty.ctype).kind.is_pointer() {
                // This calls the offset with a number literal directly, and doesn't need
                // the cast that the pointer_offset function adds
                let n = if up { one } else { mk().unary_expr(ast::UnOp::Neg, one) };
                mk().method_call_expr(read.clone(), "offset", vec![n])
            } else {
                let k = if up { BinOpKind::Add } else { BinOpKind::Sub };
                mk().binary_expr(k, read.clone(), one)
            };

        // *p = *p + rhs
        let assign_stmt = mk().assign_expr(&write, val);

        lhs_stmts.push(mk().expr_stmt(assign_stmt));

        WithStmts {
            stmts: lhs_stmts,
            val: read,
        }
    }

    fn convert_post_increment(&self, use_: ExprUse, ty: CQualTypeId, up: bool, arg: CExprId) -> WithStmts<P<Expr>> {

        // If we aren't going to be using the result, may as well do a simple pre-increment
        if use_ == ExprUse::Unused {
            return self.convert_pre_increment(ty, up, arg)
        }

        let ty = self.ast_context.index(arg).kind.get_qual_type();

        let WithStmts{ val: (write, read), stmts: mut lhs_stmts } = self.name_reference_write_read(arg);

        let val_name = self.renamer.borrow_mut().fresh();
        let save_old_val =
            mk().local_stmt(
                P(mk().local(mk().ident_pat(&val_name),
                             None as Option<P<Ty>>,
                             Some(read.clone())))
            );

        let one = mk().lit_expr(mk().int_lit(1, LitIntType::Unsuffixed));
        // *p + 1
        let val =
            if self.ast_context.resolve_type(ty.ctype).kind.is_pointer() {
                let n = if up { one } else { mk().unary_expr(ast::UnOp::Neg, one) };
                mk().method_call_expr(read.clone(), "offset", vec![n])
            } else {
                let k = if up { BinOpKind::Add } else { BinOpKind::Sub };
                mk().binary_expr(k, read.clone(), one)
            };

        // *p = *p + rhs
        let assign_stmt = mk().assign_expr(&write, val);

        lhs_stmts.push(save_old_val);
        lhs_stmts.push(mk().expr_stmt(assign_stmt));

        WithStmts {
            stmts: lhs_stmts,
            val: mk().ident_expr(val_name),
        }
    }

    fn convert_unary_operator(
        &self,
        use_: ExprUse,
        name: c_ast::UnOp,
        cqual_type: CQualTypeId,
        arg: CExprId,
    ) -> WithStmts<P<Expr>> {

        let CQualTypeId { ctype, .. } = cqual_type;
        let ty = self.convert_type(ctype);
        let resolved_ctype = self.ast_context.resolve_type(ctype);

        match name {
            c_ast::UnOp::AddressOf => {

                // In this translation, there are only pointers to functions and
                // & becomes a no-op when applied to a function.

                let arg = self.convert_expr(ExprUse::LValue, arg);

                if self.is_function_pointer(ctype) {
                    arg.map(|x| mk().call_expr(mk().ident_expr("Some"), vec![x]))
                } else {
                    let mutbl = match resolved_ctype.kind {
                        CTypeKind::Pointer(pointee) if pointee.qualifiers.is_const => Mutability::Immutable,
                        _ => Mutability::Mutable,
                    };

                    arg.map(|a| {
                        let addr_of_arg = mk().set_mutbl(mutbl).addr_of_expr(a);
                        mk().cast_expr(addr_of_arg, ty)
                    })
                }
            },
            c_ast::UnOp::PreIncrement => self.convert_pre_increment(cqual_type,true, arg),
            c_ast::UnOp::PreDecrement => self.convert_pre_increment(cqual_type,false, arg),
            c_ast::UnOp::PostIncrement => self.convert_post_increment(use_, cqual_type,true, arg),
            c_ast::UnOp::PostDecrement => self.convert_post_increment(use_, cqual_type,false, arg),
            c_ast::UnOp::Deref => {
                self.convert_expr(ExprUse::RValue, arg).map(|val: P<Expr>| {

                    let mut val = mk().unary_expr(ast::UnOp::Deref, val);

                    // If the type on the other side of the pointer we are dereferencing is volatile and
                    // this whole expression is not an LValue, we should make this a volatile read
                    if use_ != ExprUse::LValue && cqual_type.qualifiers.is_volatile {
                        val = self.volatile_read(&val, ctype)
                    }

                    val
                })
            },
            c_ast::UnOp::Plus => self.convert_expr(ExprUse::RValue, arg), // promotion is explicit in the clang AST

            c_ast::UnOp::Negate => {
                let WithStmts { val: arg, stmts } = self.convert_expr(ExprUse::RValue, arg);

                let val = if resolved_ctype.kind.is_unsigned_integral_type() {
                    mk().method_call_expr(arg, "wrapping_neg", vec![] as Vec<P<Expr>>)
                } else {
                    mk().unary_expr(ast::UnOp::Neg, arg)
                };

                WithStmts { val, stmts }
            }
            c_ast::UnOp::Complement =>
                self.convert_expr(ExprUse::RValue, arg).map(|a| mk().unary_expr(ast::UnOp::Not, a)),

            c_ast::UnOp::Not => {
                let WithStmts { val: arg, stmts } = self.convert_expr(ExprUse::RValue, arg);
                WithStmts { val:self.convert_not(ctype, arg), stmts }
            },
        }
    }

    /// Translate an assignment binary operator
    fn convert_assignment_operator(
        &self,
        use_: ExprUse,
        op: c_ast::BinOp,
        ty: P<Ty>,
        ctype: CTypeId,
        lhs: CExprId,
        rhs: CExprId,
    ) -> WithStmts<P<Expr>> {
        let lhs_type = self.ast_context.index(lhs).kind.get_qual_type();
        let rhs_type = self.ast_context.index(rhs).kind.get_qual_type();

        let is_volatile = lhs_type.qualifiers.is_volatile;
        let is_volatile_compound_assign = op.underlying_assignment().is_some() && is_volatile;

        let (write, read, lhs_stmts) = if use_ == ExprUse::RValue || is_volatile_compound_assign {
            let WithStmts { val: (write, read), stmts: lhs_stmts } = self.name_reference_write_read(lhs);
            (write, read, lhs_stmts)
        } else {
            let WithStmts { val: write, stmts: lhs_stmts } = self.name_reference_write(lhs);
            (write, Translation::panic(), lhs_stmts)
        };

        let WithStmts { val: rhs, stmts: rhs_stmts } = self.convert_expr(ExprUse::RValue, rhs);

        // Side effects to accumulate
        let mut stmts = vec![];
        stmts.extend(lhs_stmts);
        stmts.extend(rhs_stmts);

        // Assignment expression itself
        let assign_stmt = match op {
            // Regular (possibly volatile) assignment
            c_ast::BinOp::Assign if !is_volatile => mk().assign_expr(&write, rhs),
            c_ast::BinOp::Assign => self.volatile_write(&write, lhs_type.ctype, rhs),

            // Anything volatile needs to be desugared into explicit reads and writes
            op if is_volatile => {
                let op = op.underlying_assignment().expect("Cannot convert non-assignment operator");

                let val = self.convert_binary_operator(op, ty, ctype, lhs_type, rhs_type, read.clone(), rhs);

                self.volatile_write(&write, lhs_type.ctype, val)
            },

            // Everything else
            c_ast::BinOp::AssignAdd => mk().assign_op_expr(BinOpKind::Add, &write, rhs),
            c_ast::BinOp::AssignSubtract => mk().assign_op_expr(BinOpKind::Sub, &write, rhs),
            c_ast::BinOp::AssignMultiply => mk().assign_op_expr(BinOpKind::Mul, &write, rhs),
            c_ast::BinOp::AssignDivide => mk().assign_op_expr(BinOpKind::Div, &write, rhs),
            c_ast::BinOp::AssignModulus => mk().assign_op_expr(BinOpKind::Rem, &write, rhs),
            c_ast::BinOp::AssignBitXor => mk().assign_op_expr(BinOpKind::BitXor, &write, rhs),
            c_ast::BinOp::AssignShiftLeft => mk().assign_op_expr(BinOpKind::Shl, &write, rhs),
            c_ast::BinOp::AssignShiftRight => mk().assign_op_expr(BinOpKind::Shr, &write, rhs),
            c_ast::BinOp::AssignBitOr => mk().assign_op_expr(BinOpKind::BitOr, &write, rhs),
            c_ast::BinOp::AssignBitAnd => mk().assign_op_expr(BinOpKind::BitAnd, &write, rhs),

            _ => panic!("Cannot convert non-assignment operator"),
        };

        stmts.push(mk().expr_stmt(assign_stmt));

        WithStmts { stmts, val: read }
    }

    /// Translate a non-assignment binary operator. It is expected that the `lhs` and `rhs`
    /// arguments be usable as rvalues.
    fn convert_binary_operator(
        &self,
        op: c_ast::BinOp,
        ty: P<Ty>,
        ctype: CTypeId,
        lhs_type: CQualTypeId,
        rhs_type: CQualTypeId,
        lhs: P<Expr>,
        rhs: P<Expr>,
    ) -> P<Expr> {
        let is_unsigned_integral_type = self.ast_context.index(ctype).kind.is_unsigned_integral_type();

        match op {
            c_ast::BinOp::Add => self.convert_addition(lhs_type, rhs_type, lhs, rhs),
            c_ast::BinOp::Subtract => self.convert_subtraction(ty, lhs_type, rhs_type, lhs, rhs),

            c_ast::BinOp::Multiply if is_unsigned_integral_type =>
                mk().method_call_expr(lhs, mk().path_segment("wrapping_mul"), vec![rhs]),
            c_ast::BinOp::Multiply => mk().binary_expr(BinOpKind::Mul, lhs, rhs),

            c_ast::BinOp::Divide if is_unsigned_integral_type =>
                mk().method_call_expr(lhs, mk().path_segment("wrapping_div"), vec![rhs]),
            c_ast::BinOp::Divide => mk().binary_expr(BinOpKind::Div, lhs, rhs),

            c_ast::BinOp::Modulus if is_unsigned_integral_type =>
                mk().method_call_expr(lhs, mk().path_segment("wrapping_rem"), vec![rhs]),
            c_ast::BinOp::Modulus =>mk().binary_expr(BinOpKind::Rem, lhs, rhs),

            c_ast::BinOp::BitXor => mk().binary_expr(BinOpKind::BitXor, lhs, rhs),

            c_ast::BinOp::ShiftRight => mk().binary_expr(BinOpKind::Shr, lhs, rhs),
            c_ast::BinOp::ShiftLeft => mk().binary_expr(BinOpKind::Shl, lhs, rhs),

            c_ast::BinOp::EqualEqual => bool_to_int(mk().binary_expr(BinOpKind::Eq, lhs, rhs)),
            c_ast::BinOp::NotEqual => bool_to_int(mk().binary_expr(BinOpKind::Ne, lhs, rhs)),
            c_ast::BinOp::Less => bool_to_int(mk().binary_expr(BinOpKind::Lt, lhs, rhs)),
            c_ast::BinOp::Greater => bool_to_int(mk().binary_expr(BinOpKind::Gt, lhs, rhs)),
            c_ast::BinOp::GreaterEqual => bool_to_int(mk().binary_expr(BinOpKind::Ge, lhs, rhs)),
            c_ast::BinOp::LessEqual => bool_to_int(mk().binary_expr(BinOpKind::Le, lhs, rhs)),

            c_ast::BinOp::BitAnd => mk().binary_expr(BinOpKind::BitAnd, lhs, rhs),
            c_ast::BinOp::BitOr => mk().binary_expr(BinOpKind::BitOr, lhs, rhs),

            op => unimplemented!("Translation of binary operator {:?}", op),
        }
    }

    fn convert_addition(
        &self,
        lhs_type_id: CQualTypeId,
        rhs_type_id: CQualTypeId,
        lhs: P<Expr>,
        rhs: P<Expr>
    ) -> P<Expr> {
        let lhs_type = &self.ast_context.resolve_type(lhs_type_id.ctype).kind;
        let rhs_type = &self.ast_context.resolve_type(rhs_type_id.ctype).kind;

        if lhs_type.is_pointer() {
            pointer_offset(lhs, rhs)
        } else if rhs_type.is_pointer() {
            pointer_offset(lhs, rhs)
        } else if lhs_type.is_unsigned_integral_type() {
            mk().method_call_expr(lhs, mk().path_segment("wrapping_add"), vec![rhs])
        } else {
            mk().binary_expr(BinOpKind::Add, lhs, rhs)
        }
    }

    fn convert_subtraction(
        &self,
        ty: P<Ty>,
        lhs_type_id: CQualTypeId,
        rhs_type_id: CQualTypeId,
        lhs: P<Expr>,
        rhs: P<Expr>,
    ) -> P<Expr> {
        let lhs_type = &self.ast_context.resolve_type(lhs_type_id.ctype).kind;
        let rhs_type = &self.ast_context.resolve_type(rhs_type_id.ctype).kind;

        if rhs_type.is_pointer() {
            // offset_to returns None when a pointer
            // offset_opt := rhs.offset_to(lhs)
            let offset_opt = mk().method_call_expr(rhs, "offset_to", vec![lhs]);
            // msg := "bad offset_to"
            let msg = mk().lit_expr(mk().str_lit("bad offset_to"));
            // offset := offset_opt.expect(msg)
            let offset = mk().method_call_expr(offset_opt, "expect", vec![msg]);
            mk().cast_expr(offset, ty)
        } else if lhs_type.is_pointer() {
            let neg_rhs = mk().unary_expr(ast::UnOp::Neg, rhs);
            pointer_offset(lhs, neg_rhs)
        } else if lhs_type.is_unsigned_integral_type() {
            mk().method_call_expr(lhs, mk().path_segment("wrapping_sub"), vec![rhs])
        } else {
            mk().binary_expr(BinOpKind::Sub, lhs, rhs)
        }
    }

    /// Convert a boolean expression to a boolean for use in && or || or if
    fn match_bool(&self, target: bool, ty_id: CTypeId, val: P<Expr>) -> P<Expr> {
        let ty = &self.ast_context.resolve_type(ty_id).kind;

        if self.is_function_pointer(ty_id) {
            if target {
                mk().method_call_expr(val, "is_some", vec![] as Vec<P<Expr>>)
            } else {
                mk().method_call_expr(val, "is_none", vec![] as Vec<P<Expr>>)
            }
        } else if ty.is_pointer() {
            let mut res = mk().method_call_expr(val, "is_null", vec![] as Vec<P<Expr>>);
            if target {
                res = mk().unary_expr(ast::UnOp::Not, res)
            }
            res
        } else {
            let zero = if ty.is_floating_type() {
                mk().lit_expr(mk().float_unsuffixed_lit("0."))
            } else {
                mk().lit_expr(mk().int_lit(0, LitIntType::Unsuffixed))
            };

            // One simplification we can make at the cost of inspecting `val` more closely: if `val`
            // is already in the form `(x <op> y) as <ty>` where `<op>` is a Rust operator
            // that returns a boolean, we can simple output `x <op> y` or `!(x <op> y)`.
            if let ExprKind::Cast(ref arg, _) = val.node {
                if let ExprKind::Binary(op, _, _) = arg.node {
                    match op.node {
                        BinOpKind::Or | BinOpKind::And |
                        BinOpKind::Eq | BinOpKind::Ne |
                        BinOpKind::Lt | BinOpKind::Le |
                        BinOpKind::Gt | BinOpKind::Ge => return arg.clone(),
                        _ => { }
                    }
                }
            }

            // The backup is to just compare against zero
            if target {
                mk().binary_expr(BinOpKind::Ne, zero, val)
            } else {
                mk().binary_expr(BinOpKind::Eq, zero, val)
            }
        }
    }

    /// Convert expression to c_int using '!' behavior
    fn convert_not(&self, ty_id: CTypeId, val: P<Expr>) -> P<Expr> {
        let ty = &self.ast_context.resolve_type(ty_id).kind;

        let b = if ty.is_pointer() {
            mk().method_call_expr(val, "is_null", vec![] as Vec<P<Expr>>)
        } else {
            let zero = mk().lit_expr(mk().int_lit(0, LitIntType::Unsuffixed));
            mk().binary_expr(BinOpKind::Eq, zero, val)
        };

        mk().cast_expr(b, mk().path_ty(vec!["libc","c_int"]))
    }

    fn is_function_pointer(&self, typ: CTypeId) -> bool {
        let resolved_ctype = self.ast_context.resolve_type(typ);
        if let CTypeKind::Pointer(p) = resolved_ctype.kind {
            if let CTypeKind::Function { .. } = self.ast_context.resolve_type(p.ctype).kind {
                true
            } else { false }
        } else { false }
    }
}
