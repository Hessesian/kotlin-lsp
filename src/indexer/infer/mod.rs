//! Type-inference helpers for the Kotlin indexer.
//!
//! Each submodule handles one class of inference:
//! - `lambda` — decomposing lambda/function types (`(T) -> R`, receiver lambdas, etc.)
//!
//! Additional submodules (`sig`, `args`, `it_this`) will be added in subsequent steps.

pub mod lambda;

pub(crate) use lambda::{
    SCOPE_FUNCTIONS,
    lambda_type_first_input,
    lambda_type_nth_input,
    lambda_type_receiver,
};
