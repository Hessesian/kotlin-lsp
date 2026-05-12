use std::collections::HashMap;
use std::sync::Arc;

use tower_lsp::lsp_types::{Position, Range, Url};

use super::{IndexRead, WorkspaceRead};
use crate::indexer::Location;
use crate::types::FileData;

/// Minimal `WorkspaceRead` stub for unit tests.
#[derive(Default)]
struct TestWorkspace {
    definitions: HashMap<String, Vec<Location>>,
}

impl TestWorkspace {
    fn with_definition(mut self, name: &str, location: Location) -> Self {
        self.definitions.insert(name.to_owned(), vec![location]);
        self
    }
}

impl IndexRead for TestWorkspace {
    fn get_definitions(&self, name: &str) -> Option<Vec<Location>> {
        self.definitions.get(name).cloned()
    }

    fn get_file_data(&self, _uri: &str) -> Option<Arc<FileData>> {
        None
    }
}

impl WorkspaceRead for TestWorkspace {}

#[test]
fn find_definition_qualified_default_resolves_via_index() {
    let target = location("/workspace/src/Foo.kt", 3);
    let workspace = TestWorkspace::default().with_definition("Foo", target.clone());

    let result =
        workspace.find_definition_qualified("Foo", None, &file_url("/workspace/src/Bar.kt"));

    assert_eq!(result, vec![target]);
}

#[test]
fn find_definition_qualified_returns_empty_when_unknown() {
    let workspace = TestWorkspace::default();
    let result =
        workspace.find_definition_qualified("Unknown", None, &file_url("/workspace/src/Bar.kt"));
    assert!(result.is_empty());
}

fn file_url(path: &str) -> Url {
    Url::parse(&format!("file://{path}")).expect("valid file URL")
}

fn location(path: &str, line: u32) -> Location {
    Location {
        uri: file_url(path),
        range: Range::new(Position::new(line, 0), Position::new(line, 3)),
    }
}
