//! Tests for [`find_definition`] — CST-resolved-first, NameScan-fallback.

use tower_lsp::lsp_types::{GotoDefinitionResponse, Position, Url};

use crate::backend::cursor::CursorContext;
use crate::features::definition::find_definition;
use crate::indexer::Indexer;

/// House decoy: a call-expression receiver (`getUser().save()`). The
/// string/word-based qualifier extraction (`word_and_qualifier_at`) only
/// captures a simple identifier immediately before the dot — it can't
/// capture `getUser()` as a qualifier at all (the char before the dot is
/// `)`, not an identifier char), so today's path treats `save` as a BARE,
/// receiver-less reference and falls through to an unqualified same-name
/// scan that can't distinguish `User.save` from `Admin.save`. The CST path
/// walks the actual `navigation_expression` and resolves the call
/// receiver's return type directly, so it must land on `User.save` only.
#[tokio::test]
async fn goto_definition_resolves_call_expression_receiver_via_cst() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/D.kt").unwrap();
    let src = "class User { fun save() {} }\n\
               class Admin { fun save() {} }\n\
               fun getUser(): User = User()\n\
               fun f() { getUser().save() }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let col = src.lines().nth(3).unwrap().find("save").unwrap() as u32;
    let ctx = CursorContext::build(&idx, &uri, Position::new(3, col)).unwrap();
    let response = find_definition(&ctx, &idx, &uri, Position::new(3, col))
        .await
        .unwrap();
    let loc = match response {
        GotoDefinitionResponse::Scalar(l) => l,
        other => panic!("expected a single location, got {other:?}"),
    };
    assert_eq!(
        loc.range.start.line, 0,
        "must jump to User.save, not Admin.save"
    );
}

/// The reported bug: `fun <T> Flow<T>.collect(scope, block) { collect(block) }`
/// — the inner 1-arg `collect(block)` must not goto-definition back to the
/// enclosing 2-required-arg declaration just because it's the only same-named
/// symbol in the file. Exercises the whole path: `call_shape_at_callee`'s CST
/// classification, `find_definition_for_call`, and `resolve_callee_definition`'s
/// arity filter together — not just the resolver internals directly.
#[tokio::test]
async fn goto_definition_does_not_resolve_wrong_arity_call_to_enclosing_self() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Flow.kt").unwrap();
    let src = "class CoroutineScope\n\
               fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
                   collect(block)\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let col = src.lines().nth(2).unwrap().find("collect").unwrap() as u32;
    let position = Position::new(2, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    if let Some(GotoDefinitionResponse::Scalar(loc)) = &response {
        assert_ne!(
            loc.range.start.line, 1,
            "must not jump back to the enclosing declaration itself, got: {response:?}"
        );
    }
    if let Some(GotoDefinitionResponse::Array(locs)) = &response {
        assert!(
            !locs.iter().any(|loc| loc.range.start.line == 1),
            "must not include the enclosing declaration among the results, got: {response:?}"
        );
    }
}

/// Goto-definition runs constantly on mid-edit, ERROR-recovered buffers.
/// `call_shape_at_callee` is the first caller of `call_shape_of` that starts
/// from an arbitrary live cursor position rather than a node already known to
/// enclose complete syntax — verify an unterminated call argument list (the
/// user is still typing) degrades safely rather than miscounting and wrongly
/// excluding the one legitimate candidate.
#[tokio::test]
async fn goto_definition_on_unterminated_call_still_resolves() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Incomplete.kt").unwrap();
    let src = "fun greet(name: String) {}\n\
               fun test() {\n\
                   greet(name\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);
    let col = src.lines().nth(2).unwrap().find("greet").unwrap() as u32;
    let position = Position::new(2, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => {
            assert_eq!(
                loc.range.start.line, 0,
                "an unterminated call must still resolve its one legitimate \
                 candidate, not be wrongly excluded by a miscounted shape"
            );
        }
        other => panic!("expected a single resolved location, got: {other:?}"),
    }
}

/// The follow-up reported bug, once the self-shadow above was suppressed:
/// with the self-declaration correctly excluded, goto-definition landed on
/// nothing at all, because nothing ever tried "this bare call is really
/// `this.collect(...)` against the enclosing extension function's own
/// receiver" — the real target here is `Flow`'s own JAR-indexed interface
/// member, reachable only via implicit-receiver resolution (Kotlin SAM-
/// converts `block` to `FlowCollector<T>`). Exercises the whole path end to
/// end through `find_definition`, not `resolve_implicit_receiver_callee`
/// directly.
#[tokio::test]
async fn goto_definition_resolves_implicit_receiver_call_to_jar_member() {
    use crate::types::{FileData, SourceSet, SymbolEntry, Visibility};
    use std::sync::Arc;

    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Flow.kt").unwrap();
    let src = "package com.example\n\
               import kotlinx.coroutines.flow.Flow\n\
               class CoroutineScope\n\
               fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
                   collect(block)\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);

    let jar_uri_str = "jar:file:///fake-coroutines.jar!/Flow.kt".to_string();
    let jar_uri = Url::parse(&jar_uri_str).unwrap();
    let range = tower_lsp::lsp_types::Range {
        start: Position::new(0, 0),
        end: Position::new(0, 7),
    };
    let member = SymbolEntry {
        name: "collect".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::METHOD,
        visibility: Visibility::Public,
        range,
        selection_range: range,
        detail: "suspend fun collect(collector: FlowCollector<T>)".to_owned(),
        container: Some("Flow".to_owned()),
        params: "collector: FlowCollector<T>".to_owned(),
        param_counts: (1, 1),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    let flow_type = SymbolEntry {
        name: "Flow".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::INTERFACE,
        visibility: Visibility::Public,
        range,
        selection_range: range,
        detail: "interface Flow<T>".to_owned(),
        container: None,
        params: String::new(),
        param_counts: (0, 0),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    idx.jar_files.insert(
        jar_uri_str.clone(),
        Arc::new(FileData {
            symbols: vec![flow_type, member],
            source_set: SourceSet::Library,
            package: Some("kotlinx.coroutines.flow".to_owned()),
            lines: Arc::new(vec![]),
            ..Default::default()
        }),
    );
    idx.jar_definitions
        .entry("Flow".to_owned())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: jar_uri.clone(),
            range,
        });

    let col = src.lines().nth(4).unwrap().find("collect").unwrap() as u32;
    let position = Position::new(4, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    let loc = match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        Some(GotoDefinitionResponse::Array(mut locs)) if locs.len() == 1 => locs.remove(0),
        other => panic!("expected exactly one resolved location, got: {other:?}"),
    };
    assert_eq!(
        loc.uri, jar_uri,
        "must resolve to Flow's own JAR-indexed collect member via the \
         implicit `this` receiver, not the arity-incompatible self-declaration, \
         got: {loc:?}"
    );
}

/// A second, distinct manifestation of the same self-shadow bug, reported
/// after the implicit-receiver fix above shipped: an *explicit*-receiver call
/// (`triggers.collect { trigger -> }`, trailing-lambda-only — no `scope` arg
/// at all) goes through a completely different pipeline
/// (`classify_cursor`/`resolve_identity`'s CST-resolved path, not
/// `call_shape_at_callee`/`find_definition_for_call`) that had no arity
/// awareness whatsoever: `resolve_identity` resolved the receiver's type
/// (`Flow`) and searched extensions-in-scope on it completely unfiltered,
/// finding the arity-incompatible self-declaration before ever considering
/// the real member.
#[tokio::test]
async fn goto_definition_resolves_explicit_receiver_call_to_jar_member_not_self() {
    use crate::types::{FileData, SourceSet, SymbolEntry, Visibility};
    use std::sync::Arc;

    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Flow.kt").unwrap();
    let src = "package com.example\n\
               import kotlinx.coroutines.flow.Flow\n\
               class CoroutineScope\n\
               fun <T : Any> Flow<T>.collect(scope: CoroutineScope, block: (T) -> Unit) {\n\
                   collect(block)\n\
               }\n\
               fun useTriggers(triggers: Flow<String>) {\n\
                   triggers.collect { trigger -> println(trigger) }\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);

    let jar_uri_str = "jar:file:///fake-coroutines.jar!/Flow.kt".to_string();
    let jar_uri = Url::parse(&jar_uri_str).unwrap();
    let type_range = tower_lsp::lsp_types::Range {
        start: Position::new(0, 0),
        end: Position::new(0, 4),
    };
    let member_range = tower_lsp::lsp_types::Range {
        start: Position::new(1, 0),
        end: Position::new(1, 7),
    };
    let member = SymbolEntry {
        name: "collect".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::METHOD,
        visibility: Visibility::Public,
        range: member_range,
        selection_range: member_range,
        detail: "suspend fun collect(collector: FlowCollector<T>)".to_owned(),
        container: Some("Flow".to_owned()),
        params: "collector: FlowCollector<T>".to_owned(),
        param_counts: (1, 1),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    let flow_type = SymbolEntry {
        name: "Flow".to_owned(),
        kind: tower_lsp::lsp_types::SymbolKind::INTERFACE,
        visibility: Visibility::Public,
        range: type_range,
        selection_range: type_range,
        detail: "interface Flow<T>".to_owned(),
        container: None,
        params: String::new(),
        param_counts: (0, 0),
        cold: crate::types::pack_cold_fields(vec![], String::new(), String::new(), String::new()),
        trailing_lambda: false,
        deprecated: false,
    };
    idx.jar_files.insert(
        jar_uri_str.clone(),
        Arc::new(FileData {
            symbols: vec![flow_type, member],
            source_set: SourceSet::Library,
            package: Some("kotlinx.coroutines.flow".to_owned()),
            lines: Arc::new(vec![]),
            ..Default::default()
        }),
    );
    idx.jar_definitions
        .entry("Flow".to_owned())
        .or_default()
        .push(tower_lsp::lsp_types::Location {
            uri: jar_uri.clone(),
            range: type_range,
        });

    let col = src.lines().nth(7).unwrap().find("collect").unwrap() as u32;
    let position = Position::new(7, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    let loc = match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        Some(GotoDefinitionResponse::Array(mut locs)) if locs.len() == 1 => locs.remove(0),
        other => panic!("expected exactly one resolved location, got: {other:?}"),
    };
    assert_eq!(
        loc.uri, jar_uri,
        "must resolve `triggers.collect {{ trigger -> }}` to Flow's own \
         JAR-indexed collect member, not the arity-incompatible \
         collect(scope, block) self-declaration, got: {loc:?}"
    );
}

/// Real Moneta bug (`navController.navigate(route = ...)`, `NavHostController`
/// extends `NavController`): the receiver's CONCRETE type declares its own
/// same-named member with a DIFFERENT, incompatible arity (`act()`, no args —
/// standing in for `NavController.navigate(Uri)`/`navigate(NavDirections)`),
/// while the call actually wants a same-named extension declared on an
/// ancestor type (`Base.act(label: String)`, standing in for the Kotlin KTX
/// `NavController.navigate(route: String, ...)` extension). Before the fix,
/// `resolve_qualified` returned the wrong-arity member alone and never even
/// looked at the supertype-walk extension fallback, so — once the caller's
/// arity filter (correctly) rejected that member — resolution came up empty
/// entirely instead of falling through to the extension. The receiver here is
/// a plain lowercase local parameter (`d: Derived`), exercising both the
/// CST-resolved path (`classify_cursor`/`resolve_identity`) AND `find_definition`'s
/// own final string-qualifier fallback, since both now need the appended
/// extension candidate to pick the right one via shape filtering.
#[tokio::test]
async fn goto_definition_prefers_supertype_extension_over_wrong_arity_concrete_member() {
    let idx = Indexer::new();
    let uri = Url::parse("file:///t/Nav.kt").unwrap();
    let src = "open class Base\n\
               fun Base.act(label: String): Unit = TODO()\n\
               class Derived : Base() {\n\
                   fun act(): Unit = TODO()\n\
               }\n\
               fun f(d: Derived) {\n\
                   d.act(\"x\")\n\
               }\n";
    idx.index_content(&uri, src);
    idx.store_live_tree(&uri, src);

    let col = src.lines().nth(6).unwrap().find("act").unwrap() as u32;
    let position = Position::new(6, col);
    let ctx = CursorContext::build(&idx, &uri, position).unwrap();
    let response = find_definition(&ctx, &idx, &uri, position).await;
    let loc = match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => loc,
        Some(GotoDefinitionResponse::Array(mut locs)) if locs.len() == 1 => locs.remove(0),
        other => panic!("expected exactly one resolved location, got: {other:?}"),
    };
    assert_eq!(
        loc.range.start.line, 1,
        "d.act(\"x\") (1 arg) must resolve to Base's extension act(label: \
         String) on line 1, not Derived's own 0-arg act() on line 3, got: {loc:?}"
    );
}
