//! Type-inference helpers for the Kotlin indexer.
//!
//! Each submodule handles one class of inference:
//! - `deps`     — `InferDeps` trait + `TestDeps` stub for unit-testing leaf helpers
//! - `lambda`   — decomposing lambda/function types (`(T) -> R`, receiver lambdas, etc.)
//! - `sig`      — function signature extraction (pure string/slice functions)
//! - `args`     — call argument parsing (pure)
//! - `it_this`  — resolving `it`/`this` element types inside Kotlin lambda bodies

pub mod deps;
pub mod lambda;
pub mod sig;
pub mod args;
pub mod it_this;

pub(crate) use deps::InferDeps;
#[cfg(test)]
pub(crate) use deps::TestDeps;

pub(crate) use lambda::{
    RECEIVER_THIS_FNS,
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
    cst_call_fn_name,
    cst_named_arg_label,
    cst_value_arg_position,
};

pub(crate) use it_this::{
    find_it_element_type,
    find_it_element_type_in_lines,
    find_this_element_type_in_lines,
    find_named_lambda_param_type_in_lines,
    find_named_lambda_param_type,
    is_lambda_param,
    lambda_receiver_type_from_context,
    line_has_lambda_param,
    lambda_brace_pos_for_param,
    find_last_dot_at_depth_zero,
};
