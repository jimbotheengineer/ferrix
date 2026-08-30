//! `LAMBDA` and `LET` — first-class function values and lexical bindings.
//!
//! ## Why LAMBDA is not a `Value`
//!
//! `ferrix_core::Value` is the per-cell columnar type, `Copy`, and pinned to 16
//! bytes (see `.github/AGENT_GUIDE.md`, invariant "Value <= 16 bytes"). A
//! LAMBDA is a closure over a body expression plus a captured environment —
//! there is no way to fit that in 16 `Copy` bytes, and a bare cell must never
//! *be* a lambda. So, exactly like an [`crate::array::ArrayData`] result, a
//! LAMBDA lives ONLY on the evaluation side: it is produced by an `Expr::Lambda`
//! node and consumed by applying it, and it never round-trips through the
//! store. Excel takes the same stance — a cell is a value; a lambda is either
//! *named* (workbook metadata, stored as source text) or *invoked in place*.
//!
//! ## The two special forms
//!
//! - `LET(name1, value1, ..., body)` is pure lexical sugar: bind each name to
//!   its (lazily-evaluated) value, then evaluate `body` in the extended scope.
//!   It is NOT a rewrite pass — bindings live on a scope stack the evaluator
//!   consults for [`crate::parser::Expr::Var`], so a name shadows an outer one
//!   without any text substitution that could capture the wrong reference.
//! - `LAMBDA(param1, ..., body)` is a first-class function value. Evaluating an
//!   `Expr::Lambda` captures the *current* scope (closure capture) alongside the
//!   parameter list and body. Applying it binds the arguments to the parameters
//!   on top of the captured scope and evaluates the body there.
//!
//! ## Closure capture and cheap cloning
//!
//! A [`LambdaValue`] holds its parameters, body, and captured environment
//! behind an [`Rc`], so cloning one through an [`crate::array::EvalResult`] or a
//! scope binding is a refcount bump, not a deep copy of the body AST. The
//! evaluator is single-threaded per cell (spill paints from one host), so `Rc`
//! rather than `Arc` is correct and cheaper.

use std::cell::RefCell;
use std::rc::Rc;

use crate::array::EvalResult;
use crate::eval::{eval_view_array, CellSource};
use crate::parser::Expr;
use ferrix_core::{ErrorKind, Value};

/// A first-class function value: parameter names, a body expression, and the
/// environment captured at the point the `LAMBDA(...)` expression was
/// evaluated (closure capture over the enclosing LET/LAMBDA scope).
///
/// Shared behind an [`Rc`] so it threads through [`EvalResult`] and scope
/// bindings by refcount bump, never a body clone.
#[derive(Clone, Debug)]
pub struct LambdaValue(Rc<LambdaInner>);

#[derive(Debug)]
struct LambdaInner {
    params: Vec<String>,
    body: Expr,
    /// The scope in force when this LAMBDA was evaluated. `None` is the empty
    /// top-level environment; nesting shares frames by `Rc`, so a deeply nested
    /// closure does not copy its ancestors.
    captured: Option<Rc<ScopeFrame>>,
}

impl LambdaValue {
    /// Build a closure capturing `captured` as its defining environment.
    pub fn new(params: Vec<String>, body: Expr, captured: Option<Rc<ScopeFrame>>) -> Self {
        LambdaValue(Rc::new(LambdaInner {
            params,
            body,
            captured,
        }))
    }

    /// The parameter names, in declaration order.
    #[inline]
    pub fn params(&self) -> &[String] {
        &self.0.params
    }

    /// The body expression evaluated when the lambda is applied.
    #[inline]
    pub fn body(&self) -> &Expr {
        &self.0.body
    }

    /// The environment captured at definition — the base a call extends with
    /// its argument bindings.
    #[inline]
    pub fn captured(&self) -> Option<Rc<ScopeFrame>> {
        self.0.captured.clone()
    }

    /// How many parameters the lambda declares — the arity a call must match.
    #[inline]
    pub fn arity(&self) -> usize {
        self.0.params.len()
    }
}

/// Two `LambdaValue`s are equal iff they share the same allocation. Comparing
/// closures structurally is neither meaningful (two textually identical bodies
/// captured in different environments are different functions) nor needed —
/// this exists only so [`EvalResult`] can keep deriving `PartialEq` for tests
/// that compare *scalar* results.
impl PartialEq for LambdaValue {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// What a lexical name (LET binding or LAMBDA parameter) stands for.
///
/// A binding is either an already-materialised value/array, or a lambda value
/// (so a LET can name a lambda, and a lambda can be passed as an argument to
/// another lambda). Kept distinct from `EvalResult` because a lambda is not a
/// spillable data shape — it is a callable — and letting it masquerade as data
/// would put it one implicit-intersection away from being painted into a cell.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    /// A concrete evaluated value or array.
    Value(EvalResult),
    /// A callable closure.
    Lambda(LambdaValue),
}

/// One frame of the lexical scope: a set of `name -> Binding` entries and a
/// link to the enclosing frame. Shared by [`Rc`] so a captured environment and
/// a call frame extending it never copy their ancestors.
///
/// Lookup is innermost-first: a name in this frame shadows the same name in a
/// parent, which is exactly LET/LAMBDA shadowing semantics. Frames are tiny
/// (a LET binds a handful of names; Excel caps at 126), so a linear scan is
/// faster than a hash map and allocates nothing extra.
#[derive(Debug)]
pub struct ScopeFrame {
    names: Vec<(String, Binding)>,
    parent: Option<Rc<ScopeFrame>>,
}

impl ScopeFrame {
    /// Extend `parent` with one frame's worth of bindings. An empty binding set
    /// is still a valid (transparent) frame, but callers avoid pushing one.
    pub fn extend(parent: Option<Rc<ScopeFrame>>, names: Vec<(String, Binding)>) -> Rc<ScopeFrame> {
        Rc::new(ScopeFrame { names, parent })
    }

    /// Resolve `name` innermost-first. Case-insensitive to match the tokenizer,
    /// which upper-cases identifiers, and Excel's case-insensitive names.
    pub fn lookup(&self, name: &str) -> Option<Binding> {
        let mut frame = Some(self);
        while let Some(f) = frame {
            if let Some((_, b)) = f.names.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
                return Some(b.clone());
            }
            frame = f.parent.as_deref();
        }
        None
    }

    /// Length of the parent chain including this frame — the lexical nesting
    /// depth. Used to bound recursive LAMBDA application (see [`MAX_CALL_DEPTH`])
    /// so an accidentally self-referential lambda cannot recurse until the
    /// stack overflows and aborts the process.
    fn depth(&self) -> usize {
        let mut n = 1;
        let mut frame = self.parent.as_deref();
        while let Some(f) = frame {
            n += 1;
            frame = f.parent.as_deref();
        }
        n
    }
}

/// Deepest lexical nesting a chain of LET/LAMBDA frames may reach before an
/// application is refused as `#NUM!`.
///
/// v0.3 does not support recursion (a named LAMBDA calling itself); this guard
/// is what makes an accidental cycle — `LET(f, LAMBDA(n, f(n)), f(1))` — fail
/// LOUDLY as `#NUM!` instead of recursing until the stack overflows, which on a
/// release build (`panic = "unwind"`) is still an abort that discards the
/// user's unsaved edits. The limit is generous: real nested LET/LAMBDA never
/// approaches it, and the parser already caps expression nesting separately.
const MAX_CALL_DEPTH: usize = 256;

thread_local! {
    /// The lexical scope stack for the CURRENT thread's evaluation. LET/LAMBDA
    /// evaluation is a recursive descent on one thread (spill paints from a
    /// single host cell), so a thread-local stack threads the scope through the
    /// unchanged `eval_view`/`eval_view_array` recursion without a scope
    /// parameter on every call or a `CellSource` wrapper type — either of which
    /// would ripple through dozens of `?Sized`-generic signatures and every
    /// `CellSource` implementor. The top frame is the scope in force; an
    /// `Expr::Var` reads it, and a LET/LAMBDA pushes a frame for the duration of
    /// its body and pops it on the way out (see [`ScopeGuard`]).
    ///
    /// Bounded by nesting DEPTH (a handful of LET/LAMBDA levels), never by row
    /// count: each frame holds a few evaluated bindings, not a sheet column.
    static SCOPE: RefCell<Vec<Rc<ScopeFrame>>> = const { RefCell::new(Vec::new()) };
}

/// The scope frame currently in force, or `None` at the top level.
fn current_scope() -> Option<Rc<ScopeFrame>> {
    SCOPE.with(|s| s.borrow().last().cloned())
}

/// Pushes a scope frame for the lifetime of the guard and pops it on drop, so a
/// frame is removed however the body evaluation exits — including on a panic
/// unwinding through it, which a bare push/pop pair would leak, corrupting the
/// scope of whatever the unwind lands in.
struct ScopeGuard;

impl ScopeGuard {
    fn push(frame: Rc<ScopeFrame>) -> ScopeGuard {
        SCOPE.with(|s| s.borrow_mut().push(frame));
        ScopeGuard
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        SCOPE.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// Resolve a lexical variable through the current thread's scope. `None` when
/// there is no scope (a plain sheet evaluation) or the name is not bound — the
/// caller renders both as `#NAME?`.
pub fn lookup_var(name: &str) -> Option<Binding> {
    current_scope()?.lookup(name)
}

/// Evaluate a sub-expression to a [`Binding`] — the form a LET value or a
/// LAMBDA argument takes on the scope stack.
///
/// A `LAMBDA(...)` (or a variable already bound to a lambda) becomes a
/// `Binding::Lambda`, so it can be named by a LET or passed as an argument to
/// another lambda and later invoked. Everything else is evaluated in array
/// context and stored as a `Binding::Value`, preserving an array so a LET can
/// name a spilled result.
fn eval_binding<S: CellSource + ?Sized>(expr: &Expr, src: &S) -> Binding {
    match expr {
        // A literal lambda captures the CURRENT scope (closure capture over the
        // enclosing LET/LAMBDA) rather than being evaluated to a value.
        Expr::Lambda(params, body) => Binding::Lambda(LambdaValue::new(
            params.clone(),
            (**body).clone(),
            current_scope(),
        )),
        // A variable that resolves to a lambda threads the lambda through
        // unchanged, so `LET(g, f, ...)` names the same closure `f` names.
        Expr::Var(name) => match lookup_var(name) {
            Some(b) => b,
            None => Binding::Value(EvalResult::Scalar(Value::Error(ErrorKind::Name))),
        },
        // Anything else is data. Evaluate in array context so a spilled result
        // survives being named.
        _ => Binding::Value(eval_view_array(expr, src)),
    }
}

/// Evaluate `LET(bindings, body)`: bind each name to its value in the scope
/// built by the bindings before it, then evaluate `body` in the fully-extended
/// scope. Bindings are added one frame at a time so an earlier name is visible
/// to a later binding's value (Excel's top-to-bottom LET semantics).
pub fn eval_let<S: CellSource + ?Sized>(
    bindings: &[(String, Expr)],
    body: &Expr,
    src: &S,
) -> EvalResult {
    // Layer one frame per binding on top of the current scope so an earlier
    // name is in scope for a later binding's value AND for the body. Each guard
    // stays alive until the body is evaluated, then unwinds in reverse.
    let mut guards: Vec<ScopeGuard> = Vec::with_capacity(bindings.len());
    for (name, value_expr) in bindings {
        let binding = eval_binding(value_expr, src);
        let frame = ScopeFrame::extend(current_scope(), vec![(name.clone(), binding)]);
        guards.push(ScopeGuard::push(frame));
    }
    let result = eval_view_array(body, src);
    // Explicit for clarity — the guards would drop here regardless, popping
    // this LET's frames so a sibling expression never sees its bindings.
    drop(guards);
    result
}

/// Evaluate `callee(args)`: resolve the callee to a lambda, bind the arguments
/// to its parameters over its CAPTURED environment (closure semantics, not the
/// caller's scope), and evaluate the body there.
pub fn eval_apply<S: CellSource + ?Sized>(callee: &Expr, args: &[Expr], src: &S) -> EvalResult {
    // The callee must evaluate to a lambda. A literal `LAMBDA(...)` captures the
    // current scope; a variable must be bound to a lambda; anything else is not
    // callable.
    let lambda = match callee {
        Expr::Lambda(params, body) => {
            LambdaValue::new(params.clone(), (**body).clone(), current_scope())
        }
        Expr::Var(name) => match lookup_var(name) {
            Some(Binding::Lambda(l)) => l,
            // A variable bound to a value is not callable; an unbound name is
            // `#NAME?`.
            Some(Binding::Value(_)) => return EvalResult::Scalar(Value::Error(ErrorKind::Value)),
            None => return EvalResult::Scalar(Value::Error(ErrorKind::Name)),
        },
        // Applying a non-lambda expression (e.g. `(1+2)(3)`) is `#VALUE!`.
        _ => return EvalResult::Scalar(Value::Error(ErrorKind::Value)),
    };

    // Arity must match exactly, like Excel: too few or too many arguments is an
    // error rather than a silent pad/truncate.
    if args.len() != lambda.arity() {
        return EvalResult::Scalar(Value::Error(ErrorKind::Value));
    }

    // Recursion guard: refuse before the captured chain plus this call frame
    // could overflow the stack. The captured chain's depth grows every time a
    // self-referential lambda re-applies, so this trips on a runaway cycle.
    let captured_depth = lambda.captured().as_deref().map_or(0, ScopeFrame::depth);
    if captured_depth >= MAX_CALL_DEPTH {
        return EvalResult::Scalar(Value::Error(ErrorKind::Num));
    }

    // Evaluate each argument in the CALLER's scope (which is what is in force
    // right now), then bind it to the corresponding parameter over the lambda's
    // CAPTURED scope. Evaluating args in the caller's scope and the body in the
    // closure's is exactly lexical closure semantics.
    let bound: Vec<(String, Binding)> = lambda
        .params()
        .iter()
        .zip(args.iter())
        .map(|(param, arg)| (param.clone(), eval_binding(arg, src)))
        .collect();
    let frame = ScopeFrame::extend(lambda.captured(), bound);
    let _guard = ScopeGuard::push(frame);
    eval_view_array(lambda.body(), src)
}
