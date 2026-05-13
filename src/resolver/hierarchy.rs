//! Class hierarchy traversal — walk supertypes for member resolution.

use std::collections::HashSet;

use crate::indexer::Indexer;
use crate::types::{CallerContext, FileData};

/// Walk the class hierarchy starting from `start_class`, collecting items at each level.
/// `T` is what the visitor produces per symbol. `max_depth` prevents infinite loops.
pub(crate) fn walk_hierarchy<'a, T, F>(
    idx: &'a Indexer,
    start_class: &str,
    start_uri: &str,
    caller: CallerContext<'a>,
    max_depth: usize,
    collect: F,
) -> Vec<T>
where
    F: Fn(&Indexer, &str, &str, CallerContext<'_>) -> Vec<T>,
{
    let mut walker = HierarchyWalker {
        idx,
        caller,
        max_depth,
        collect,
        visited: HashSet::from([(start_uri.to_owned(), start_class.to_owned())]),
        items: Vec::new(),
    };
    walker.recurse(start_class, start_uri, 0);
    walker.items
}

struct HierarchyWalker<'a, T, F>
where
    F: Fn(&Indexer, &str, &str, CallerContext<'_>) -> Vec<T>,
{
    idx: &'a Indexer,
    caller: CallerContext<'a>,
    max_depth: usize,
    collect: F,
    visited: HashSet<(String, String)>,
    items: Vec<T>,
}

impl<'a, T, F> HierarchyWalker<'a, T, F>
where
    F: Fn(&Indexer, &str, &str, CallerContext<'_>) -> Vec<T>,
{
    fn recurse(&mut self, class_name: &str, class_uri: &str, depth: usize) {
        if depth >= self.max_depth {
            return;
        }

        for (super_name, super_uri) in supertype_targets(self.idx, class_name, class_uri) {
            if !self.visited.insert((super_uri.clone(), super_name.clone())) {
                continue;
            }
            self.items.extend((self.collect)(
                self.idx,
                &super_name,
                &super_uri,
                self.caller,
            ));
            self.recurse(&super_name, &super_uri, depth + 1);
        }
    }
}

fn supertype_targets(idx: &Indexer, class_name: &str, class_uri: &str) -> Vec<(String, String)> {
    use tower_lsp::lsp_types::Url;
    let Ok(uri) = Url::parse(class_uri) else {
        return vec![];
    };
    let Some(file_data) = super::ensure_file_data(idx, &uri) else {
        return vec![];
    };

    super_names_for_class(&file_data, class_name)
        .into_iter()
        .flat_map(|super_name| {
            super::resolve_symbol_inner(idx, &super_name, &uri, false)
                .into_iter()
                .map(move |loc| (super_name.clone(), loc.uri.to_string()))
        })
        .collect()
}

fn super_names_for_class(file_data: &FileData, class_name: &str) -> Vec<String> {
    if class_name.is_empty() {
        return file_data
            .supers
            .iter()
            .map(|(_, name, _)| name.clone())
            .collect();
    }

    let class_line = file_data
        .symbols
        .iter()
        .find(|symbol| symbol.name == class_name)
        .map(|symbol| symbol.selection_start());
    match class_line {
        Some(line) => file_data
            .supers
            .iter()
            .filter(|(super_line, _, _)| *super_line == line)
            .map(|(_, name, _)| name.clone())
            .collect(),
        None => file_data
            .supers
            .iter()
            .map(|(_, name, _)| name.clone())
            .collect(),
    }
}
