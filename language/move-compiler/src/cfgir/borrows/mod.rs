// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use move_ir_types::location::*;
use state::{Value, *};

use crate::{
    diagnostics::Diagnostics,
    hlir::ast::*,
    parser::ast::{BinOp_, StructName, Var},
    shared::{unique_map::UniqueMap, CompilationEnv},
};

use super::absint::*;

mod state;

//**************************************************************************************************
// Entry and trait bindings
//**************************************************************************************************

struct BorrowSafety {
    local_numbers: UniqueMap<Var, usize>,
}

impl BorrowSafety {
    fn new<T>(local_types: &UniqueMap<Var, T>) -> Self {
        let mut local_numbers = UniqueMap::new();
        for (idx, (v, _)) in local_types.key_cloned_iter().enumerate() {
            local_numbers.add(v, idx).unwrap();
        }
        Self { local_numbers }
    }
}

struct Context<'a, 'b> {
    local_numbers: &'a UniqueMap<Var, usize>,
    borrow_state: &'b mut BorrowState,
    diags: Diagnostics,
}

impl<'a, 'b> Context<'a, 'b> {
    fn new(safety: &'a BorrowSafety, borrow_state: &'b mut BorrowState) -> Self {
        let local_numbers = &safety.local_numbers;
        Self {
            local_numbers,
            borrow_state,
            diags: Diagnostics::new(),
        }
    }

    fn get_diags(self) -> Diagnostics {
        self.diags
    }

    fn add_diags(&mut self, additional: Diagnostics) {
        self.diags.extend(additional);
    }
}

impl TransferFunctions for BorrowSafety {
    type State = BorrowState;

    fn execute(
        &mut self,
        pre: &mut Self::State,
        _lbl: Label,
        _idx: usize,
        cmd: &Command,
    ) -> Diagnostics {
        let mut context = Context::new(self, pre);
        command(&mut context, cmd);
        context
            .borrow_state
            .canonicalize_locals(context.local_numbers);
        context.get_diags()
    }
}

impl AbstractInterpreter for BorrowSafety {}

pub fn verify(
    compilation_env: &mut CompilationEnv,
    signature: &FunctionSignature,
    acquires: &BTreeMap<StructName, Loc>,
    locals: &UniqueMap<Var, SingleType>,
    cfg: &super::cfg::BlockCFG,
) -> BTreeMap<Label, BorrowState> {
    // check for existing errors
    let has_errors = compilation_env.has_errors();
    let mut initial_state = BorrowState::initial(locals, acquires.clone(), has_errors);
    initial_state.bind_arguments(&signature.parameters);
    let mut safety = BorrowSafety::new(locals);
    initial_state.canonicalize_locals(&safety.local_numbers);
    let (final_state, ds) = safety.analyze_function(cfg, initial_state);
    compilation_env.add_diags(ds);
    final_state
}

//**************************************************************************************************
// Command
//**************************************************************************************************

fn command(context: &mut Context, sp!(loc, cmd_): &Command) {
    use Command_ as C;
    match cmd_ {
        C::Assign(ls, e) => {
            let values = exp(context, e);
            lvalues(context, ls, values);
        }
        C::Mutate(el, er) => {
            let value = assert_single_value(exp(context, er));
            assert!(!value.is_ref());
            let lvalue = assert_single_value(exp(context, el));
            let diags = context.borrow_state.mutate(*loc, lvalue);
            context.add_diags(diags);
        }
        C::JumpIf { cond: e, .. } => {
            let value = assert_single_value(exp(context, e));
            assert!(!value.is_ref());
        }
        C::IgnoreAndPop { exp: e, .. } => {
            let values = exp(context, e);
            context.borrow_state.release_values(values);
        }

        C::Return { exp: e, .. } => {
            let values = exp(context, e);
            let diags = context.borrow_state.return_(*loc, values);
            context.add_diags(diags);
        }
        C::Abort(e) => {
            let value = assert_single_value(exp(context, e));
            assert!(!value.is_ref());
            context.borrow_state.abort()
        }
        C::Jump { .. } => (),
        C::Break | C::Continue => panic!("ICE break/continue not translated to jumps"),
    }
}

fn lvalues(context: &mut Context, ls: &[LValue], values: Values) {
    ls.iter()
        .zip(values)
        .for_each(|(l, value)| lvalue(context, l, value))
}

fn lvalue(context: &mut Context, sp!(loc, l_): &LValue, value: Value) {
    use LValue_ as L;
    match l_ {
        L::Ignore => {
            context.borrow_state.release_value(value);
        }
        L::Var(v, _) => {
            let diags = context.borrow_state.assign_local(*loc, v, value);
            context.add_diags(diags)
        }
        L::Unpack(_, _, fields) => {
            assert!(!value.is_ref());
            fields
                .iter()
                .for_each(|(_, l)| lvalue(context, l, Value::NonRef))
        }
    }
}

fn exp(context: &mut Context, parent_e: &Exp) -> Values {
    use UnannotatedExp_ as E;
    let eloc = &parent_e.exp.loc;
    let svalue = || vec![Value::NonRef];
    match &parent_e.exp.value {
        E::Move { var, annotation } => {
            let last_usage = matches!(annotation, MoveOpAnnotation::InferredLastUsage);
            let (diags, value) = context.borrow_state.move_local(*eloc, var, last_usage);
            context.add_diags(diags);
            vec![value]
        }
        E::Copy { var, .. } => {
            let (diags, value) = context.borrow_state.copy_local(*eloc, var);
            context.add_diags(diags);
            vec![value]
        }
        E::BorrowLocal(mut_, var) => {
            let (diags, value) = context.borrow_state.borrow_local(*eloc, *mut_, var);
            context.add_diags(diags);
            assert!(value.is_ref());
            vec![value]
        }
        E::Freeze(e) => {
            let evalue = assert_single_value(exp(context, e));
            let (diags, value) = context.borrow_state.freeze(*eloc, evalue);
            context.add_diags(diags);
            vec![value]
        }
        E::Dereference(e) => {
            let evalue = assert_single_value(exp(context, e));
            let (errors, value) = context.borrow_state.dereference(*eloc, evalue);
            context.add_diags(errors);
            vec![value]
        }
        E::Borrow(mut_, e, f) => {
            let evalue = assert_single_value(exp(context, e));
            let (diags, value) = context.borrow_state.borrow_field(*eloc, *mut_, evalue, f);
            context.add_diags(diags);
            vec![value]
        }

        E::Builtin(b, e) => {
            let evalues = exp(context, e);
            let b: &BuiltinFunction = b;
            match b {
                sp!(_, BuiltinFunction_::BorrowGlobal(mut_, t)) => {
                    assert!(!assert_single_value(evalues).is_ref());
                    let (diags, value) = context.borrow_state.borrow_global(*eloc, *mut_, t);
                    context.add_diags(diags);
                    vec![value]
                }
                sp!(_, BuiltinFunction_::MoveFrom(t)) => {
                    assert!(!assert_single_value(evalues).is_ref());
                    let (diags, value) = context.borrow_state.move_from(*eloc, t);
                    assert!(!value.is_ref());
                    context.add_diags(diags);
                    vec![value]
                }
                _ => {
                    let ret_ty = &parent_e.ty;
                    let (diags, values) =
                        context
                            .borrow_state
                            .call(*eloc, evalues, &BTreeMap::new(), ret_ty);
                    context.add_diags(diags);
                    values
                }
            }
        }

        E::Vector(_, n, _, e) => {
            let evalues = exp(context, e);
            debug_assert_eq!(*n, evalues.len());
            evalues.into_iter().for_each(|v| assert!(!v.is_ref()));
            svalue()
        }

        E::ModuleCall(mcall) => {
            let evalues = exp(context, &mcall.arguments);
            let ret_ty = &parent_e.ty;
            let (diags, values) =
                context
                    .borrow_state
                    .call(*eloc, evalues, &mcall.acquires, ret_ty);
            context.add_diags(diags);
            values
        }

        E::Unit { .. } => vec![],
        E::Value(_) | E::Constant(_) | E::Spec(_, _, _) | E::UnresolvedError => svalue(),

        E::Cast(e, _) | E::UnaryExp(_, e) => {
            let v = exp(context, e);
            assert!(!assert_single_value(v).is_ref());
            svalue()
        }
        E::BinopExp(e1, sp!(_, BinOp_::Eq), e2) | E::BinopExp(e1, sp!(_, BinOp_::Neq), e2) => {
            let v1 = assert_single_value(exp(context, e1));
            let v2 = assert_single_value(exp(context, e2));
            // must check separately incase of using a local with an unassigned value
            if v1.is_ref() {
                let (errors, _) = context.borrow_state.dereference(e1.exp.loc, v1);
                assert!(errors.is_empty(), "ICE eq freezing failed");
            }
            if v2.is_ref() {
                let (errors, _) = context.borrow_state.dereference(e1.exp.loc, v2);
                assert!(errors.is_empty(), "ICE eq freezing failed");
            }
            svalue()
        }
        E::BinopExp(e1, _, e2) => {
            let v1 = assert_single_value(exp(context, e1));
            let v2 = assert_single_value(exp(context, e2));
            assert!(!v1.is_ref());
            assert!(!v2.is_ref());
            svalue()
        }
        E::Pack(_, _, fields) => {
            fields.iter().for_each(|(_, _, e)| {
                let arg = exp(context, e);
                assert!(!assert_single_value(arg).is_ref());
            });
            svalue()
        }

        E::ExpList(es) => es
            .iter()
            .flat_map(|item| exp_list_item(context, item))
            .collect(),

        E::Unreachable => panic!("ICE should not analyze dead code"),
    }
}

fn exp_list_item(context: &mut Context, item: &ExpListItem) -> Values {
    match item {
        ExpListItem::Single(e, _) | ExpListItem::Splat(_, e, _) => exp(context, e),
    }
}
