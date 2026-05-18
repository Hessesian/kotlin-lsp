//! Type-inference helpers for the Kotlin indexer.
//!
//! Each submodule handles one class of inference:
//! - `deps`        — `InferDeps` trait + `TestDeps` stub for unit-testing leaf helpers
//! - `lambda`      — decomposing lambda/function types (`(T) -> R`, receiver lambdas, etc.)
//! - `sig`         — function signature extraction (pure string/slice functions)
//! - `args`        — call argument parsing (pure)
//! - `it_this`     — resolving `it`/`this` element types inside Kotlin lambda bodies
//! - `type_subst`  — generic type-parameter substitution
//! - `chain`       — CST navigation-chain type resolution
//! - `receiver`    — lambda receiver type inference from text context
//! - `cst_lambda`  — CST-backed ThisLambdaCtx + lambda context helpers

pub(super) mod args;
pub(super) mod chain;
pub(super) mod cst_cursor;
pub(super) mod cst_lambda;
pub(super) mod deps;
pub(super) mod expr_type;
pub(super) mod it_this;
pub(super) mod lambda;
pub(super) mod receiver;
pub(super) mod sig;
pub(super) mod type_subst;
