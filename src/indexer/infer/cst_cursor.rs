use tower_lsp::lsp_types::{Position, Url};
use tree_sitter::Point;

use crate::indexer::live_tree::utf16_col_to_byte;
use crate::indexer::{Indexer, NodeExt};
use crate::queries::{
    KIND_ANON_FUN, KIND_CALL_EXPR, KIND_CLASS_BODY, KIND_COMPANION_OBJ, KIND_FUN_DECL,
    KIND_LAMBDA_LIT, KIND_METHOD_DECL, KIND_MULTI_VAR_DECL, KIND_NAV_EXPR, KIND_OBJECT_DECL,
    KIND_PROP_DECL, KIND_SOURCE_FILE, KIND_VALUE_ARG, KIND_VAR_DECL,
};

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallInfo {
    pub fn_name: String,
    pub qualifier: Option<String>,
    pub active_param: u32,
}

#[allow(dead_code)]
pub(crate) fn cst_call_info(pos: Position, indexer: &Indexer, uri: &Url) -> Option<CallInfo> {
    let doc = indexer.live_doc(uri)?;
    let bytes = &doc.bytes;
    let full_text = std::str::from_utf8(bytes).ok()?;

    let line_idx = pos.line as usize;
    let line_text = full_text.lines().nth(line_idx)?;
    let byte_col = utf16_col_to_byte(line_text, pos.character as usize);
    let point = Point {
        row: line_idx,
        column: byte_col,
    };
    let start_node = doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;

    let mut cur = start_node;
    let call_expr = loop {
        match cur.kind() {
            KIND_CALL_EXPR => break Some(cur),
            KIND_LAMBDA_LIT => break None,
            _ => match cur.parent() {
                Some(parent) => cur = parent,
                None => break None,
            },
        }
    }?;

    let (fn_name, qualifier) = call_expr.call_fn_and_qualifier(bytes)?;
    let value_arguments = call_expr.find_value_arguments()?;
    let cursor_byte = full_text
        .lines()
        .take(line_idx)
        .map(|line| line.len() + 1)
        .sum::<usize>()
        + byte_col;
    let active_param = {
        let mut count = 0u32;
        let mut walker = value_arguments.walk();
        for child in value_arguments.children(&mut walker) {
            if child.kind() == KIND_VALUE_ARG {
                if child.end_byte() <= cursor_byte {
                    count += 1;
                } else {
                    break;
                }
            }
        }
        count
    };

    Some(CallInfo {
        fn_name,
        qualifier,
        active_param,
    })
}

#[allow(dead_code)]
pub(crate) fn cst_cursor_is_local_var(indexer: &Indexer, uri: &Url, pos: Position) -> bool {
    let doc = match indexer.live_doc(uri) {
        Some(doc) => doc,
        None => return false,
    };
    let full_text = match std::str::from_utf8(&doc.bytes) {
        Ok(text) => text,
        Err(_) => return false,
    };
    let line_idx = pos.line as usize;
    let line_text = match full_text.lines().nth(line_idx) {
        Some(line) => line,
        None => return false,
    };
    let byte_col = utf16_col_to_byte(line_text, pos.character as usize);
    let point = Point {
        row: line_idx,
        column: byte_col,
    };
    let start_node = match doc
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    {
        Some(node) => node,
        None => return false,
    };

    let mut in_binding = false;
    let mut cur = start_node;
    loop {
        match cur.kind() {
            KIND_PROP_DECL | KIND_VAR_DECL | KIND_MULTI_VAR_DECL => {
                in_binding = true;
            }
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN | KIND_LAMBDA_LIT if in_binding => {
                return true;
            }
            KIND_FUN_DECL | KIND_METHOD_DECL | KIND_ANON_FUN | KIND_LAMBDA_LIT => return false,
            KIND_NAV_EXPR => return false,
            KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ | KIND_SOURCE_FILE
                if in_binding =>
            {
                return false;
            }
            KIND_CLASS_BODY | KIND_OBJECT_DECL | KIND_COMPANION_OBJ | KIND_SOURCE_FILE => {
                return false;
            }
            _ => {}
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return false,
        }
    }
}
