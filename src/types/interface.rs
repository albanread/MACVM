//! T1 (`docs/typechecker_design.md` §5): a static, VM-free model of the
//! world's class interfaces, built by re-parsing every `.mst` file
//! `world.list` names with the ordinary frontend parser — "the checker
//! parses the world with the ordinary frontend," no `VmState`, nothing
//! executed. Deliberately does NOT reuse `frontend::world::load_world`'s
//! own file-listing loop: that one is tightly coupled to
//! `classdef::execute_top_item` (a live VM), and separating "list the
//! files" from "load each file into a VM" would mean refactoring the real
//! world-boot path for this checker's sake — out of proportion for a
//! ~20-line duplicate of world.list's blank/comment/duplicate rules.

use std::collections::HashMap;
use std::path::Path;

use crate::frontend::ast::{ClassDefNode, MethodNode, TopItem};
use crate::frontend::parser;

/// One class's declared interface, as captured from its `.mst` source(s).
/// A class reopened across multiple files (`world/*.mst`'s own convention)
/// accumulates methods across every reopening — later wins for a
/// redefinition, the same "latest wins" semantics the running VM's own
/// method-dictionary install uses. `inst_vars`/`inst_var_types` are set by
/// whichever declaration provides a NON-EMPTY ivar list — matching real
/// semantics (`frontend::classdef::install_class_def`'s reopen path
/// REJECTS a reopen that supplies its own non-empty ivar list as "cannot
/// change shape of existing class"), so in valid source at most one
/// declaration ever has one to give.
pub struct ClassInterface {
    pub name: String,
    /// `None` for the nil-superclass root (`nil subclass: Object`); the
    /// SUPERCLASS NAME is not itself validated as a real class here — a
    /// dangling superclass reference is a compile error the real VM
    /// already catches (`classdef::resolve_super_target`), not this
    /// checker's job to re-diagnose.
    pub superclass: Option<String>,
    pub inst_vars: Vec<String>,
    /// Parallel to `inst_vars` by index — the raw captured annotation TEXT
    /// (not yet parsed into a `TypeExpr`; `check::check_world` parses each
    /// lazily as it walks).
    pub inst_var_types: Vec<Option<String>>,
    pub instance_methods: HashMap<String, MethodNode>,
    pub class_methods: HashMap<String, MethodNode>,
}

impl ClassInterface {
    fn new(name: String) -> Self {
        ClassInterface {
            name,
            superclass: None,
            inst_vars: Vec::new(),
            inst_var_types: Vec::new(),
            instance_methods: HashMap::new(),
            class_methods: HashMap::new(),
        }
    }

    fn merge(&mut self, node: ClassDefNode) {
        self.superclass = if node.superclass == "nil" {
            None
        } else {
            Some(node.superclass)
        };
        if !node.inst_vars.is_empty() {
            self.inst_vars = node.inst_vars;
            self.inst_var_types = node
                .inst_var_types
                .into_iter()
                .map(|o| o.map(|t| t.text))
                .collect();
        }
        for m in node.methods {
            let dict = if m.class_side {
                &mut self.class_methods
            } else {
                &mut self.instance_methods
            };
            dict.insert(m.pattern_selector.clone(), m);
        }
    }
}

/// The whole world's static interface model.
pub struct WorldModel {
    pub classes: HashMap<String, ClassInterface>,
    /// Load order (first-declaration order), for reproducible iteration —
    /// a `HashMap`'s own iteration order isn't, and a report that reorders
    /// itself between runs is annoying to diff.
    pub class_order: Vec<String>,
}

impl WorldModel {
    fn add_or_reopen(&mut self, node: ClassDefNode) {
        if !self.classes.contains_key(&node.name) {
            self.class_order.push(node.name.clone());
            self.classes
                .insert(node.name.clone(), ClassInterface::new(node.name.clone()));
        }
        self.classes
            .get_mut(&node.name)
            .expect("just inserted or already present")
            .merge(node);
    }
}

/// Reads `world_dir/world.list` and parses every listed `.mst` file, folding
/// each `ClassDef` into the returned model (`TopItem::DoIt` top-level
/// statements are IGNORED — the checker analyzes declarations, never
/// executes anything). Mirrors `frontend::world::load_world`'s blank/
/// comment/duplicate-entry rules for `world.list` itself.
pub fn build_world_model(world_dir: &Path) -> Result<WorldModel, String> {
    let list_path = world_dir.join("world.list");
    let list_src = std::fs::read_to_string(&list_path)
        .map_err(|e| format!("cannot read {}: {e}", list_path.display()))?;

    let mut model = WorldModel {
        classes: HashMap::new(),
        class_order: Vec::new(),
    };
    let mut seen = std::collections::HashSet::new();
    for (i, raw_line) in list_src.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !seen.insert(line.to_string()) {
            return Err(format!(
                "{}:{}: duplicate world.list entry '{line}'",
                list_path.display(),
                i + 1
            ));
        }
        let file_path = world_dir.join(line);
        let text = std::fs::read_to_string(&file_path)
            .map_err(|e| format!("cannot read {}: {e}", file_path.display()))?;
        let items = parser::parse_file(&text)
            .map_err(|e| format!("{}:{}:{}: {}", file_path.display(), e.span.line, e.span.col, e.msg))?;
        for item in items {
            if let TopItem::ClassDef(c) = item {
                model.add_or_reopen(c);
            }
        }
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_world_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macvm_types_interface_test_{tag}_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn builds_a_single_class_with_ivars_and_methods() {
        let dir = temp_world_dir("single");
        fs::write(
            dir.join("01_foo.mst"),
            "Object subclass: Foo [\n\
             \x20   | count <Integer> |\n\
             \x20   bump: n <Integer> ^ <Integer> [\n\
             \x20       ^n\n\
             \x20   ]\n\
             \x20   Foo class >> zero ^ <Foo> [\n\
             \x20       ^self new\n\
             \x20   ]\n\
             ]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_foo.mst\n").unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(model.class_order, vec!["Foo".to_string()]);
        let foo = &model.classes["Foo"];
        assert_eq!(foo.superclass.as_deref(), Some("Object"));
        assert_eq!(foo.inst_vars, vec!["count".to_string()]);
        assert_eq!(foo.inst_var_types, vec![Some("Integer".to_string())]);
        assert!(foo.instance_methods.contains_key("bump:"));
        assert!(foo.class_methods.contains_key("zero"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_merges_methods_and_keeps_first_declarations_ivars() {
        // A reopen reuses the SAME `Superclass subclass: Name [...]` header
        // (`frontend::classdef` detects "already bound" -> reopen path by
        // NAME, not by a distinct keyword) — matches how real `world/*.mst`
        // files reopen a class across multiple files.
        let dir = temp_world_dir("reopen");
        fs::write(
            dir.join("01_foo.mst"),
            "Object subclass: Foo [\n    | count |\n    bar [ ^1 ]\n]\n",
        )
        .unwrap();
        fs::write(
            dir.join("02_more.mst"),
            "Object subclass: Foo [\n    baz [ ^2 ]\n]\n",
        )
        .unwrap();
        fs::write(dir.join("world.list"), "01_foo.mst\n02_more.mst\n").unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(model.class_order, vec!["Foo".to_string()]);
        let foo = &model.classes["Foo"];
        assert_eq!(foo.inst_vars, vec!["count".to_string()]);
        assert!(foo.instance_methods.contains_key("bar"));
        assert!(foo.instance_methods.contains_key("baz"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nil_superclass_is_none() {
        let dir = temp_world_dir("nilsuper");
        fs::write(dir.join("01_object.mst"), "nil subclass: Object [\n]\n").unwrap();
        fs::write(dir.join("world.list"), "01_object.mst\n").unwrap();

        let model = build_world_model(&dir).unwrap();
        assert_eq!(model.classes["Object"].superclass, None);

        fs::remove_dir_all(&dir).ok();
    }
}
