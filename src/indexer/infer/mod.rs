//! Type-inference helpers for the Kotlin indexer.
//!
//! Each submodule handles one class of inference:
//! - `lambda` — decomposing lambda/function types (`(T) -> R`, receiver lambdas, etc.)
//! - `sig`    — function signature extraction (pure string/slice functions)
//! - `args`   — call argument parsing (pure)

pub mod lambda;
pub mod sig;
pub mod args;

pub(crate) use lambda::{
    SCOPE_FUNCTIONS,
    lambda_type_first_input,
    lambda_type_nth_input,
    lambda_type_receiver,
};

pub(crate) use sig::{
    collect_signature,
    find_fun_signature_full,
    find_fun_signature_with_receiver,
    collect_all_fun_params_texts,
    nth_fun_param_type_str,
    last_fun_param_type_str,
    strip_trailing_call_args,
};

pub(crate) use args::{
    find_as_call_arg_type,
    extract_first_arg,
    extract_named_arg_name,
    find_named_param_type_in_sig,
    lambda_param_position_on_line,
    has_named_params_not_it,
};
