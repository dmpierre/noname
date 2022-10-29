use std::{
    fmt::{self, Display, Formatter},
    ops::Neg,
};

use ark_ff::{One, Zero};
use kimchi::circuits::polynomials::generic::{GENERIC_COEFFS, GENERIC_REGISTERS};
use num_bigint::BigUint;
use num_traits::Num as _;

use crate::{
    asm, boolean,
    circuit_writer::{CircuitWriter, FnEnv, VarInfo},
    constants::{Field, Span, NUM_REGISTERS},
    error::{Error, ErrorKind, Result},
    field,
    imports::FnKind,
    parser::{Expr, ExprKind, Function, Op2, Stmt, StmtKind, TyKind},
    syntax::is_type,
    type_checker::{checker::TypeChecker, Dependencies, StructInfo},
    var::{CellVar, ConstOrCell, Value, Var, VarOrRef},
};

//
// Data structures
//

#[derive(Debug, Clone, Copy)]
pub enum GateKind {
    Zero,
    DoubleGeneric,
    Poseidon,
}

impl From<GateKind> for kimchi::circuits::gate::GateType {
    fn from(gate_kind: GateKind) -> Self {
        use kimchi::circuits::gate::GateType::*;
        match gate_kind {
            GateKind::Zero => Zero,
            GateKind::DoubleGeneric => Generic,
            GateKind::Poseidon => Poseidon,
        }
    }
}

// TODO: this could also contain the span that defined the gate!
#[derive(Debug)]
pub struct Gate {
    /// Type of gate
    pub typ: GateKind,

    /// Coefficients
    pub coeffs: Vec<Field>,

    /// The place in the original source code that created that gate.
    pub span: Span,

    /// A note on why this was added
    pub note: &'static str,
}

impl Gate {
    pub fn to_kimchi_gate(&self, row: usize) -> kimchi::circuits::gate::CircuitGate<Field> {
        kimchi::circuits::gate::CircuitGate {
            typ: self.typ.into(),
            wires: kimchi::circuits::wires::Wire::new(row),
            coeffs: self.coeffs.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    pub row: usize,
    pub col: usize,
}

impl Display for Cell {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})", self.row, self.col)
    }
}

#[derive(Debug, Clone)]
pub enum Wiring {
    /// Not yet wired (just indicates the position of the cell itself)
    NotWired(Cell),
    /// The wiring (associated to different spans)
    Wired(Vec<(Cell, Span)>),
}

//
// Circuit Writer (also used by witness generation)
//

impl CircuitWriter {
    /// Creates a global environment from the one created by the type checker.
    pub fn new(code: &str, typed_global_env: TypeChecker) -> Self {
        let (main_sig, main_span) = {
            let fn_info = typed_global_env.functions.get("main").cloned().unwrap();

            (fn_info.sig().clone(), fn_info.span)
        };

        Self {
            source: code.to_string(),
            main: (main_sig, main_span),
            typed: typed_global_env,
            double_generic_gate_optimization: false,
            ..Self::default()
        }
    }

    pub fn struct_info(&self, name: &str) -> Option<&StructInfo> {
        self.typed.struct_info(name)
    }

    /// Stores type information about a local variable.
    /// Note that we forbid shadowing at all scopes.
    pub fn add_constant_var(&mut self, name: String, constant: Field, span: Span) {
        let var = Var::new_constant(constant, span);

        let var_info = VarInfo::new(var, false, Some(TyKind::Field));

        if self.constants.insert(name.clone(), var_info).is_some() {
            panic!("constant `{name}` already exists (TODO: better error)");
        }
    }

    /// Retrieves type information on a constantiable, given a name.
    /// If the constantiable is not in scope, return false.
    // TODO: return an error no?
    pub fn get_constant(&self, ident: &str) -> Option<&VarInfo> {
        self.constants.get(ident)
    }

    /// Returns the compiled gates of the circuit.
    pub fn compiled_gates(&self) -> &[Gate] {
        if !self.finalized {
            panic!("Circuit not finalized yet!");
        }
        &self.gates
    }

    fn compile_stmt(
        &mut self,
        fn_env: &mut FnEnv,
        deps: &Dependencies,
        stmt: &Stmt,
    ) -> Result<Option<VarOrRef>> {
        match &stmt.kind {
            StmtKind::Assign { mutable, lhs, rhs } => {
                // compute the rhs
                let rhs_var = self
                    .compute_expr(fn_env, deps, rhs)?
                    .ok_or_else(|| Error::new(ErrorKind::CannotComputeExpression, stmt.span))?;

                // obtain the actual values
                let rhs_var = rhs_var.value(fn_env);

                let typ = self.typed.expr_type(rhs).cloned();
                let var_info = VarInfo::new(rhs_var, *mutable, typ);

                // store the new variable
                // TODO: do we really need to store that in the scope? That's not an actual var in the scope that's an internal var...
                fn_env.add_var(lhs.value.clone(), var_info);
            }

            StmtKind::ForLoop { var, range, body } => {
                for ii in range.range() {
                    fn_env.nest();

                    let cst_var = Var::new_constant(ii.into(), var.span);
                    let var_info = VarInfo::new(cst_var, false, Some(TyKind::Field));
                    fn_env.add_var(var.value.clone(), var_info);

                    self.compile_block(fn_env, deps, body)?;

                    fn_env.pop();
                }
            }
            StmtKind::Expr(expr) => {
                // compute the expression
                let var = self.compute_expr(fn_env, deps, expr)?;

                // make sure it does not return any value.
                assert!(var.is_none());
            }
            StmtKind::Return(expr) => {
                let var = self
                    .compute_expr(fn_env, deps, expr)?
                    .ok_or_else(|| Error::new(ErrorKind::CannotComputeExpression, stmt.span))?;

                // we already checked in type checking that this is not an early return
                return Ok(Some(var));
            }
            StmtKind::Comment(_) => (),
        }

        Ok(None)
    }

    /// might return something?
    fn compile_block(
        &mut self,
        fn_env: &mut FnEnv,
        deps: &Dependencies,
        stmts: &[Stmt],
    ) -> Result<Option<Var>> {
        fn_env.nest();
        for stmt in stmts {
            let res = self.compile_stmt(fn_env, deps, stmt)?;
            if let Some(var) = res {
                // a block doesn't return a pointer, only values
                let var = var.value(fn_env);

                // we already checked for early returns in type checking
                return Ok(Some(var));
            }
        }
        fn_env.pop();
        Ok(None)
    }

    fn compile_native_function_call(
        &mut self,
        deps: &Dependencies,
        function: &Function,
        args: Vec<VarInfo>,
    ) -> Result<Option<Var>> {
        assert!(!function.is_main());

        // create new fn_env
        let fn_env = &mut FnEnv::new(&self.constants);

        // set arguments
        assert_eq!(function.sig.arguments.len(), args.len());

        for (name, var_info) in function.sig.arguments.iter().zip(args) {
            fn_env.add_var(name.name.value.clone(), var_info);
        }

        // compile it and potentially return a return value
        self.compile_block(fn_env, deps, &function.body)
    }

    pub(crate) fn constrain_inputs_to_main(
        &mut self,
        input: &[ConstOrCell],
        input_typ: &TyKind,
        span: Span,
    ) {
        match input_typ {
            TyKind::Field => (),
            TyKind::Bool => {
                assert_eq!(input.len(), 1);
                boolean::check(self, &input[0], span);
            }
            TyKind::Array(tykind, _) => {
                let el_size = self.typed.size_of(tykind);
                for el in input.chunks(el_size) {
                    self.constrain_inputs_to_main(el, tykind, span);
                }
            }
            TyKind::Custom {
                module,
                name: struct_name,
            } => {
                let struct_info = self
                    .struct_info(&struct_name.value)
                    .expect("type-checker bug: couldn't find struct info of input to main")
                    .clone();
                let mut offset = 0;
                for (_field_name, field_typ) in &struct_info.fields {
                    let len = self.typed.size_of(field_typ);
                    let range = offset..(offset + len);
                    self.constrain_inputs_to_main(&input[range], field_typ, span);
                    offset += len;
                }
            }
            TyKind::BigInt => unreachable!(),
        }
    }

    /// Compile a function. Used to compile `main()` only for now
    pub(crate) fn compile_main_function(
        &mut self,
        fn_env: &mut FnEnv,
        deps: &Dependencies,
        function: &Function,
    ) -> Result<()> {
        assert!(function.is_main());

        // compile the block
        let returned = self.compile_block(fn_env, deps, &function.body)?;

        // we're expecting something returned?
        match (function.sig.return_type.as_ref(), returned) {
            (None, None) => Ok(()),
            (Some(expected), None) => Err(Error::new(ErrorKind::MissingReturn, expected.span)),
            (None, Some(returned)) => Err(Error::new(ErrorKind::UnexpectedReturn, returned.span)),
            (Some(_expected), Some(returned)) => {
                // make sure there are no constants in the returned value
                let mut returned_cells = vec![];
                for r in &returned.cvars {
                    match r {
                        ConstOrCell::Cell(c) => returned_cells.push(c),
                        ConstOrCell::Const(_) => {
                            return Err(Error::new(ErrorKind::ConstantInOutput, returned.span))
                        }
                    }
                }

                // store the return value in the public input that was created for that ^
                let public_output = self
                    .public_output
                    .as_ref()
                    .expect("bug in the compiler: missing public output");

                for (pub_var, ret_var) in public_output.cvars.iter().zip(returned_cells) {
                    // replace the computation of the public output vars with the actual variables being returned here
                    let var_idx = pub_var.idx().unwrap();
                    let prev = self
                        .witness_vars
                        .insert(var_idx, Value::PublicOutput(Some(*ret_var)));
                    assert!(prev.is_some());
                }

                Ok(())
            }
        }
    }

    pub fn asm(&self, debug: bool) -> String {
        asm::generate_asm(&self.source, &self.gates, &self.wiring, debug)
    }

    pub fn new_internal_var(&mut self, val: Value, span: Span) -> CellVar {
        // create new var
        let var = CellVar::new(self.next_variable, span);
        self.next_variable += 1;

        // store it in the circuit_writer
        self.witness_vars.insert(var.index, val);

        var
    }

    fn compute_expr(
        &mut self,
        fn_env: &mut FnEnv,
        deps: &Dependencies,
        expr: &Expr,
    ) -> Result<Option<VarOrRef>> {
        match &expr.kind {
            // `module::fn_name(args)`
            ExprKind::FnCall {
                module,
                fn_name,
                args,
            } => {
                // compute the arguments
                // module::fn_name(args)
                //                 ^^^^
                let mut vars = Vec::with_capacity(args.len());
                for arg in args {
                    // get the variable behind the expression
                    let var = self
                        .compute_expr(fn_env, deps, arg)?
                        .ok_or_else(|| Error::new(ErrorKind::CannotComputeExpression, arg.span))?;

                    // we pass variables by values always
                    let var = var.value(fn_env);

                    let typ = self.typed.expr_type(arg).cloned();
                    let mutable = false; // TODO: mut keyword in arguments?
                    let var_info = VarInfo::new(var, mutable, typ);

                    vars.push(var_info);
                }

                // retrieve the function in the env
                if let Some(module) = module {
                    // module::fn_name(args)
                    // ^^^^^^
                    let module = self.typed.modules.get(&module.value).ok_or_else(|| {
                        Error::new(
                            ErrorKind::UndefinedModule(module.value.clone()),
                            module.span,
                        )
                    })?;

                    let fn_info = deps.get_fn(module, fn_name)?;

                    match &fn_info.kind {
                        FnKind::BuiltIn(_, handle) => {
                            let res = handle(self, &vars, expr.span);
                            res.map(|r| r.map(VarOrRef::Var))
                        }
                        FnKind::Native(_) => todo!(),
                        FnKind::Main(_) => Err(Error::new(ErrorKind::RecursiveMain, expr.span)),
                    }
                } else {
                    // fn_name(args)
                    // ^^^^^^^
                    let fn_info = self
                        .typed
                        .functions
                        .get(&fn_name.value)
                        .cloned()
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::UndefinedFunction(fn_name.value.clone()),
                                fn_name.span,
                            )
                        })?;

                    match &fn_info.kind {
                        FnKind::BuiltIn(_sig, handle) => {
                            let res = handle(self, &vars, expr.span);
                            res.map(|r| r.map(VarOrRef::Var))
                        }
                        FnKind::Native(func) => {
                            let res = self.compile_native_function_call(deps, &func, vars);
                            res.map(|r| r.map(VarOrRef::Var))
                        }
                        FnKind::Main(_) => Err(Error::new(ErrorKind::RecursiveMain, expr.span)),
                    }
                }
            }

            ExprKind::FieldAccess { lhs, rhs } => {
                // get var behind lhs
                let lhs_var = self
                    .compute_expr(fn_env, deps, lhs)?
                    .ok_or_else(|| Error::new(ErrorKind::CannotComputeExpression, lhs.span))?;

                // get struct info behind lhs
                let lhs_struct = self
                    .typed
                    .expr_type(lhs)
                    .ok_or_else(|| Error::new(ErrorKind::CannotComputeExpression, lhs.span))?;

                let self_struct = match lhs_struct {
                    TyKind::Custom { module, name } => name,
                    _ => {
                        panic!("could not figure out struct implementing that method call")
                    }
                };

                let struct_info = self
                    .typed
                    .struct_info(&self_struct.value)
                    .expect("struct info not found for custom struct");

                // find range of field
                let mut start = 0;
                let mut len = 0;
                for (field, field_typ) in &struct_info.fields {
                    if field == &rhs.value {
                        len = self.typed.size_of(field_typ);
                        break;
                    }

                    start += self.typed.size_of(field_typ);
                }

                // narrow the variable to the given range
                let var = lhs_var.narrow(start, len);
                Ok(Some(var))
            }

            // `Thing.method(args)` or `thing.method(args)`
            ExprKind::MethodCall {
                lhs,
                method_name,
                args,
            } => {
                // figure out the name of the custom struct
                let lhs_typ = self
                    .typed
                    .expr_type(lhs)
                    .expect("method call on what?")
                    .clone();

                let struct_name = match &lhs_typ {
                    TyKind::Custom { module, name } => name,
                    _ => panic!("method call only work on custom types (TODO: better error)"),
                };

                // get var of `self`
                // (might be `None` if it's a static method call)
                let self_var = self.compute_expr(fn_env, deps, lhs)?;

                // find method info
                let struct_info = self
                    .typed
                    .struct_info(&struct_name.value)
                    .expect("could not find struct info")
                    .clone();

                let func = struct_info
                    .methods
                    .get(&method_name.value)
                    .expect("could not find method");

                // if method has a `self` argument, manually add it to the list of argument
                let mut vars = vec![];
                if let Some(first_arg) = func.sig.arguments.first() {
                    if first_arg.name.value == "self" {
                        let self_var = self_var.ok_or_else(|| {
                            Error::new(ErrorKind::NotAStaticMethod, method_name.span)
                        })?;

                        // TODO: for now we pass `self` by value as well
                        let mutable = false;
                        let self_var = self_var.value(fn_env);

                        let self_var_info = VarInfo::new(self_var, mutable, Some(lhs_typ.clone()));
                        vars.insert(0, self_var_info);
                    }
                } else {
                    assert!(self_var.is_none());
                }

                // compute the arguments
                for arg in args {
                    let var = self
                        .compute_expr(fn_env, deps, arg)?
                        .ok_or_else(|| Error::new(ErrorKind::CannotComputeExpression, arg.span))?;

                    // TODO: for now we pass `self` by value as well
                    let mutable = false;
                    let var = var.value(fn_env);

                    let typ = self.typed.expr_type(arg).cloned();
                    let var_info = VarInfo::new(var, mutable, typ);

                    vars.push(var_info);
                }

                // execute method
                let res = self.compile_native_function_call(deps, func, vars);
                res.map(|r| r.map(VarOrRef::Var))
            }

            ExprKind::IfElse { cond, then_, else_ } => {
                let cond = self
                    .compute_expr(fn_env, deps, cond)?
                    .unwrap()
                    .value(fn_env);
                let then_ = self
                    .compute_expr(fn_env, deps, then_)?
                    .unwrap()
                    .value(fn_env);
                let else_ = self
                    .compute_expr(fn_env, deps, else_)?
                    .unwrap()
                    .value(fn_env);

                let res = field::if_else(self, &cond, &then_, &else_, expr.span);

                Ok(Some(VarOrRef::Var(res)))
            }

            ExprKind::Assignment { lhs, rhs } => {
                // figure out the local var  of lhs
                let lhs = self.compute_expr(fn_env, deps, lhs)?.unwrap();

                // figure out the var of what's on the right
                let rhs = self.compute_expr(fn_env, deps, rhs)?.unwrap();
                let rhs_var = match rhs {
                    VarOrRef::Var(var) => var,
                    VarOrRef::Ref {
                        var_name,
                        start,
                        len,
                    } => {
                        let var_info = fn_env.get_var(&var_name);
                        let cvars = var_info.var.range(start, len).to_vec();
                        Var::new(cvars, var_info.var.span)
                    }
                };

                // replace the left with the right
                match lhs {
                    VarOrRef::Var(_) => panic!("can't reassign this non-mutable variable"),
                    VarOrRef::Ref {
                        var_name,
                        start,
                        len,
                    } => {
                        fn_env.reassign_var_range(&var_name, rhs_var, start, len);
                    }
                }

                Ok(None)
            }

            ExprKind::BinaryOp { op, lhs, rhs, .. } => {
                let lhs = self.compute_expr(fn_env, deps, lhs)?.unwrap();
                let rhs = self.compute_expr(fn_env, deps, rhs)?.unwrap();

                let lhs = lhs.value(fn_env);
                let rhs = rhs.value(fn_env);

                let res = match op {
                    Op2::Addition => field::add(self, &lhs[0], &rhs[0], expr.span),
                    Op2::Subtraction => field::sub(self, &lhs[0], &rhs[0], expr.span),
                    Op2::Multiplication => field::mul(self, &lhs[0], &rhs[0], expr.span),
                    Op2::Equality => field::equal(self, &lhs, &rhs, expr.span),
                    Op2::BoolAnd => boolean::and(self, &lhs[0], &rhs[0], expr.span),
                    Op2::BoolOr => boolean::or(self, &lhs[0], &rhs[0], expr.span),
                    Op2::Division => todo!(),
                };

                Ok(Some(VarOrRef::Var(res)))
            }

            ExprKind::Negated(b) => {
                let var = self.compute_expr(fn_env, deps, b)?.unwrap();

                let var = var.value(fn_env);

                todo!()
            }

            ExprKind::Not(b) => {
                let var = self.compute_expr(fn_env, deps, b)?.unwrap();

                let var = var.value(fn_env);

                let res = boolean::not(self, &var[0], expr.span.merge_with(b.span));
                Ok(Some(VarOrRef::Var(res)))
            }

            ExprKind::BigInt(b) => {
                let biguint = BigUint::from_str_radix(b, 10).expect("failed to parse number.");
                let ff = Field::try_from(biguint).map_err(|_| {
                    Error::new(ErrorKind::CannotConvertToField(b.to_string()), expr.span)
                })?;

                let res = VarOrRef::Var(Var::new_constant(ff, expr.span));
                Ok(Some(res))
            }

            ExprKind::Bool(b) => {
                let value = if *b { Field::one() } else { Field::zero() };
                let res = VarOrRef::Var(Var::new_constant(value, expr.span));
                Ok(Some(res))
            }

            ExprKind::Variable { module, name } => {
                if module.is_some() {
                    panic!("accessing module variables not supported yet");
                }

                if is_type(&name.value) {
                    // if it's a type we return nothing
                    // (most likely what follows is a static method call)
                    Ok(None)
                } else {
                    let var_info = fn_env.get_var(&name.value);

                    let res = VarOrRef::from_var_info(name.value.clone(), var_info);
                    Ok(Some(res))
                }
            }

            ExprKind::ArrayAccess { array, idx } => {
                // retrieve var of array
                let var = self
                    .compute_expr(fn_env, deps, array)?
                    .expect("array access on non-array");

                // compute the index
                let idx_var = self
                    .compute_expr(fn_env, deps, idx)?
                    .ok_or_else(|| Error::new(ErrorKind::CannotComputeExpression, expr.span))?;
                let idx = idx_var
                    .constant()
                    .ok_or_else(|| Error::new(ErrorKind::ExpectedConstant, expr.span))?;
                let idx: BigUint = idx.into();
                let idx: usize = idx.try_into().unwrap();

                // retrieve the type of the elements in the array
                let array_typ = self
                    .typed
                    .expr_type(array)
                    .expect("cannot find type of array");

                let elem_type = match array_typ {
                    TyKind::Array(ty, array_len) => {
                        if idx >= (*array_len as usize) {
                            return Err(Error::new(
                                ErrorKind::ArrayIndexOutOfBounds(idx, *array_len as usize - 1),
                                expr.span,
                            ));
                        }
                        ty
                    }
                    _ => panic!("expected array"),
                };

                // compute the size of each element in the array
                let len = self.typed.size_of(elem_type);

                // compute the real index
                let start = idx * len;

                // out-of-bound checks
                if start >= var.len() || start + len > var.len() {
                    return Err(Error::new(
                        ErrorKind::ArrayIndexOutOfBounds(start, var.len()),
                        expr.span,
                    ));
                }

                // index into the var
                let var = var.narrow(start, len);

                //
                Ok(Some(var))
            }

            ExprKind::ArrayDeclaration(items) => {
                let mut cvars = vec![];

                for item in items {
                    let var = self.compute_expr(fn_env, deps, item)?.unwrap();
                    let to_extend = var.value(fn_env).cvars.clone();
                    cvars.extend(to_extend);
                }

                let var = VarOrRef::Var(Var::new(cvars, expr.span));

                Ok(Some(var))
            }

            ExprKind::CustomTypeDeclaration {
                struct_name: _,
                fields,
            } => {
                // create the struct by just concatenating all of its cvars
                let mut cvars = vec![];
                for (_field, rhs) in fields {
                    let var = self.compute_expr(fn_env, deps, rhs)?.unwrap();
                    let to_extend = var.value(fn_env).cvars.clone();
                    cvars.extend(to_extend);
                }
                let var = VarOrRef::Var(Var::new(cvars, expr.span));

                //
                Ok(Some(var))
            }
        }
    }

    // TODO: dead code?
    pub fn compute_constant(&self, var: CellVar, span: Span) -> Result<Field> {
        match &self.witness_vars.get(&var.index) {
            Some(Value::Constant(c)) => Ok(*c),
            Some(Value::LinearCombination(lc, cst)) => {
                let mut res = *cst;
                for (coeff, var) in lc {
                    res += self.compute_constant(*var, span)? * *coeff;
                }
                Ok(res)
            }
            Some(Value::Mul(lhs, rhs)) => {
                let lhs = self.compute_constant(*lhs, span)?;
                let rhs = self.compute_constant(*rhs, span)?;
                Ok(lhs * rhs)
            }
            _ => Err(Error::new(ErrorKind::ExpectedConstant, span)),
        }
    }

    pub fn num_gates(&self) -> usize {
        self.gates.len()
    }

    // TODO: we should cache constants to avoid creating a new variable for each constant
    /// This should be called only when you want to constrain a constant for real.
    /// Gates that handle constants should always make sure to call this function when they want them constrained.
    pub fn add_constant(
        &mut self,
        label: Option<&'static str>,
        value: Field,
        span: Span,
    ) -> CellVar {
        if let Some(cvar) = self.cached_constants.get(&value) {
            return *cvar;
        }

        let var = self.new_internal_var(Value::Constant(value), span);
        self.cached_constants.insert(value, var);

        let zero = Field::zero();
        self.add_generic_gate(
            label.unwrap_or("hardcode a constant"),
            vec![Some(var)],
            vec![Field::one(), zero, zero, zero, value.neg()],
            span,
        );

        var
    }

    /// creates a new gate, and the associated row in the witness/execution trace.
    // TODO: add_gate instead of gates?
    pub fn add_gate(
        &mut self,
        note: &'static str,
        typ: GateKind,
        vars: Vec<Option<CellVar>>,
        coeffs: Vec<Field>,
        span: Span,
    ) {
        // sanitize
        assert!(coeffs.len() <= NUM_REGISTERS);
        assert!(vars.len() <= NUM_REGISTERS);

        // construct the execution trace with vars, for the witness generation
        self.rows_of_vars.push(vars.clone());

        // get current row
        // important: do that before adding the gate below
        let row = self.gates.len();

        // add gate
        self.gates.push(Gate {
            typ,
            coeffs,
            span,
            note,
        });

        // wiring (based on vars)
        for (col, var) in vars.iter().enumerate() {
            if let Some(var) = var {
                let curr_cell = Cell { row, col };
                self.wiring
                    .entry(var.index)
                    .and_modify(|w| match w {
                        Wiring::NotWired(cell) => {
                            *w = Wiring::Wired(vec![(*cell, var.span), (curr_cell, span)])
                        }
                        Wiring::Wired(ref mut cells) => {
                            cells.push((curr_cell, span));
                        }
                    })
                    .or_insert(Wiring::NotWired(curr_cell));
            }
        }
    }

    pub fn add_public_inputs(&mut self, name: String, num: usize, span: Span) -> Var {
        let mut cvars = Vec::with_capacity(num);

        for idx in 0..num {
            // create the var
            let cvar = self.new_internal_var(Value::External(name.clone(), idx), span);
            cvars.push(ConstOrCell::Cell(cvar));

            // create the associated generic gate
            self.add_gate(
                "add public input",
                GateKind::DoubleGeneric,
                vec![Some(cvar)],
                vec![Field::one()],
                span,
            );
        }

        self.public_input_size += num;

        Var::new(cvars, span)
    }

    pub fn add_public_outputs(&mut self, num: usize, span: Span) {
        assert!(self.public_output.is_none());

        let mut cvars = Vec::with_capacity(num);
        for _ in 0..num {
            // create the var
            let cvar = self.new_internal_var(Value::PublicOutput(None), span);
            cvars.push(ConstOrCell::Cell(cvar));

            // create the associated generic gate
            self.add_generic_gate(
                "add public output",
                vec![Some(cvar)],
                vec![Field::one()],
                span,
            );
        }
        self.public_input_size += num;

        // store it
        let res = Var::new(cvars, span);
        self.public_output = Some(res);
    }

    pub fn add_private_inputs(&mut self, name: String, num: usize, span: Span) -> Var {
        let mut cvars = Vec::with_capacity(num);

        for idx in 0..num {
            // create the var
            let cvar = self.new_internal_var(Value::External(name.clone(), idx), span);
            cvars.push(ConstOrCell::Cell(cvar));
            self.private_input_indices.push(cvar.index);
        }

        Var::new(cvars, span)
    }

    pub(crate) fn add_generic_gate(
        &mut self,
        label: &'static str,
        mut vars: Vec<Option<CellVar>>,
        mut coeffs: Vec<Field>,
        span: Span,
    ) {
        // padding
        let coeffs_padding = GENERIC_COEFFS.checked_sub(coeffs.len()).unwrap();
        coeffs.extend(std::iter::repeat(Field::zero()).take(coeffs_padding));

        let vars_padding = GENERIC_REGISTERS.checked_sub(vars.len()).unwrap();
        vars.extend(std::iter::repeat(None).take(vars_padding));

        // if the double gate optimization is not set, just add the gate
        if !self.double_generic_gate_optimization {
            self.add_gate(label, GateKind::DoubleGeneric, vars, coeffs, span);
            return;
        }

        // only add a double generic gate if we have two of them
        if let Some(generic_gate) = self.pending_generic_gate.take() {
            coeffs.extend(generic_gate.coeffs);
            vars.extend(generic_gate.vars);

            // TODO: what to do with the label and span?

            self.add_gate(label, GateKind::DoubleGeneric, vars, coeffs, span);
        } else {
            // otherwise queue it
            self.pending_generic_gate = Some(PendingGate {
                label,
                coeffs,
                vars,
                span,
            });
        }
    }
}

#[derive(Clone, Default, Debug)]
pub(crate) struct PendingGate {
    pub label: &'static str,
    pub coeffs: Vec<Field>,
    pub vars: Vec<Option<CellVar>>,
    pub span: Span,
}
