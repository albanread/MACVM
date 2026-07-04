//! A hand-authored stand-in for the real mirror/reflection layer
//! (`../../docs/APPS.md` §2-3: `ClassMirror`, `MethodMirror`, the R1/R2
//! primitive groups) — enough fake data, behind roughly the same query
//! shape those mirrors will eventually expose, to build and test the GUI's
//! class browser view (packages → classes → categories → methods → source)
//! and the GUI↔VM messaging protocol that drives it, well before core eval
//! (S5/S6) or the Smalltalk mirror library (Phase W) exist.
//!
//! Every class/method/source string here is invented for this purpose —
//! not copied from Strongtalk, Amber, or any other Smalltalk corpus this
//! project references elsewhere (`smappl.md`, `docs/reference-vm-analysis.md`).
//! This crate is throwaway scaffolding: once `VmHandle`/mirrors are real,
//! `gui`'s dependency on this crate is meant to be swapped for the real
//! thing, not extended.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Side {
    #[default]
    Instance,
    Class,
}

#[derive(Clone, Debug)]
pub struct MockMethod {
    pub selector: String,
    pub category: String,
    pub side: Side,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct MockClass {
    pub name: String,
    pub superclass: Option<String>,
    pub package: String,
    pub comment: String,
    pub instance_vars: String,
    pub class_vars: String,
    pub methods: Vec<MockMethod>,
}

/// Result of [`MockWorld::create_or_reopen_class`] — mirrors
/// `image_store::ClassCreateOutcome` (duplicated rather than depending on
/// that crate, keeping this one dependency-free per its own doc comment).
/// `Reopened` never actually comes back from this flat, tombstone-free
/// mock (a removed class is just gone from `classes`, not soft-deleted, so
/// recreating it looks identical to a fresh create) — `vm_host.rs` treats
/// the real `image_store::Image`'s outcome as authoritative for messaging
/// when an image is present, and only falls back to this one in pure-mock
/// (no image file) mode, where the distinction is cosmetic anyway.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClassCreateOutcome {
    Created,
    Reopened,
    AlreadyLive,
}

impl MockClass {
    fn categories(&self, side: Side) -> Vec<String> {
        let mut seen = Vec::new();
        for m in &self.methods {
            if m.side == side && !seen.contains(&m.category) {
                seen.push(m.category.clone());
            }
        }
        seen
    }

    fn methods_in(&self, side: Side, category: &str) -> Vec<&MockMethod> {
        self.methods.iter().filter(|m| m.side == side && m.category == category).collect()
    }

    fn methods_of(&self, side: Side) -> Vec<&MockMethod> {
        self.methods.iter().filter(|m| m.side == side).collect()
    }
}

pub struct MockWorld {
    classes: Vec<MockClass>,
}

fn method(selector: &str, category: &str, side: Side, source: &str) -> MockMethod {
    MockMethod { selector: selector.into(), category: category.into(), side, source: source.into() }
}

impl MockWorld {
    /// An empty world, built up one class/method at a time via
    /// [`add_class`](Self::add_class)/[`add_method`](Self::add_method) —
    /// for `gui/src/vm_host.rs` to mirror a real `image_store::Image`'s
    /// contents into a `MockWorld` at startup (`MACVM_IMAGE_PATH`), so
    /// `browser_render.rs` can render from either backing store without
    /// caring which one it is.
    pub fn empty() -> Self {
        Self { classes: Vec::new() }
    }

    pub fn add_class(&mut self, name: &str, superclass: Option<&str>, package: &str, comment: &str, instance_vars: &str, class_vars: &str) {
        self.classes.push(MockClass {
            name: name.to_string(),
            superclass: superclass.map(str::to_string),
            package: package.to_string(),
            comment: comment.to_string(),
            instance_vars: instance_vars.to_string(),
            class_vars: class_vars.to_string(),
            methods: Vec::new(),
        });
    }

    /// Returns `false` (no auto-create) if `class_name` isn't already
    /// present — same "reopen, don't silently create" contract
    /// `image_store::Image::add_method` uses.
    pub fn add_method(&mut self, class_name: &str, side: Side, selector: &str, category: &str, source: &str) -> bool {
        let Some(class) = self.classes.iter_mut().find(|c| c.name == class_name) else { return false };
        class.methods.push(method(selector, category, side, source));
        true
    }

    /// Remove a class outright (no tombstone — see [`ClassCreateOutcome`]'s
    /// doc comment for why this mirror doesn't need one). Leaves any other
    /// class's `superclass` string field untouched even if it named this
    /// one — same "a removed superclass counts as absent, subclasses
    /// re-root rather than vanish" rule `image_store::Image::remove_class`
    /// documents; `package_roots` below already treats a dangling
    /// superclass name the same as a missing one. Returns `false` if
    /// `name` wasn't present.
    pub fn remove_class(&mut self, name: &str) -> bool {
        let before = self.classes.len();
        self.classes.retain(|c| c.name != name);
        self.classes.len() != before
    }

    /// Returns `false` if `class_name`/`side`/`selector` doesn't name an
    /// existing method.
    pub fn remove_method(&mut self, class_name: &str, side: Side, selector: &str) -> bool {
        let Some(class) = self.class_named_mut(class_name) else { return false };
        let before = class.methods.len();
        class.methods.retain(|m| !(m.side == side && m.selector == selector));
        class.methods.len() != before
    }

    /// The class-browser "New Class" path — see
    /// `image_store::Image::create_or_reopen_class`'s doc comment for the
    /// three-way `Created`/`Reopened`/`AlreadyLive` distinction this
    /// mirrors (modulo `Reopened` never firing here, per
    /// [`ClassCreateOutcome`]'s doc comment).
    pub fn create_or_reopen_class(&mut self, name: &str, superclass: Option<&str>, package: &str, comment: &str, instance_vars: &str) -> ClassCreateOutcome {
        if self.class_named(name).is_some() {
            return ClassCreateOutcome::AlreadyLive;
        }
        self.add_class(name, superclass, package, comment, instance_vars, "");
        ClassCreateOutcome::Created
    }

    /// The class-browser "New Method" path — always succeeds (creating,
    /// reopening, or redefining as needed) as long as `class_name` exists;
    /// see `image_store::Image::create_or_reopen_method`'s doc comment for
    /// why none of those three cases should be treated as an error.
    /// Returns `false` only if `class_name` doesn't exist.
    pub fn create_or_reopen_method(&mut self, class_name: &str, side: Side, selector: &str, category: &str, source: &str) -> bool {
        let Some(class) = self.class_named_mut(class_name) else { return false };
        match class.methods.iter_mut().find(|m| m.side == side && m.selector == selector) {
            Some(m) => {
                m.category = category.to_string();
                m.source = source.to_string();
            }
            None => class.methods.push(method(selector, category, side, source)),
        }
        true
    }

    /// "Accept" an edited class definition (superclass + instance
    /// variables) — see `image_store::Image::set_class_definition`'s doc
    /// comment for why `class_vars` isn't included.
    pub fn set_class_definition(&mut self, class_name: &str, new_superclass: Option<String>, new_instance_vars: String) -> bool {
        let Some(class) = self.class_named_mut(class_name) else { return false };
        class.superclass = new_superclass;
        class.instance_vars = new_instance_vars;
        true
    }

    /// A small, invented class hierarchy across two packages — enough
    /// shape (multiple packages, a real subclass tree, instance *and*
    /// class-side methods, several categories per class) to exercise every
    /// pane of the browser view without needing a large seed.
    pub fn seed() -> Self {
        let classes = vec![
            MockClass {
                name: "Object".into(),
                superclass: None,
                package: "Kernel".into(),
                comment: "The root of the class hierarchy. Defines default behavior shared by every object.".into(),
                instance_vars: "".into(),
                class_vars: "".into(),
                methods: vec![
                    method("printString", "printing", Side::Instance,
                        "printString\n\t^String streamContents: [:s | self printOn: s]"),
                    method("printOn:", "printing", Side::Instance,
                        "printOn: aStream\n\taStream nextPutAll: self class name"),
                    method("=", "comparing", Side::Instance,
                        "= other\n\t^self == other"),
                    method("hash", "comparing", Side::Instance,
                        "hash\n\t^self identityHash"),
                    method("new", "instance creation", Side::Class,
                        "new\n\t^self basicNew initialize"),
                ],
            },
            MockClass {
                name: "Collection".into(),
                superclass: Some("Object".into()),
                package: "Collections".into(),
                comment: "Abstract superclass for objects that hold a group of elements.".into(),
                instance_vars: "".into(),
                class_vars: "".into(),
                methods: vec![
                    method("do:", "iterating", Side::Instance,
                        "do: aBlock\n\tself subclassResponsibility"),
                    method("size", "accessing", Side::Instance,
                        "size\n\t| n |\n\tn := 0.\n\tself do: [:each | n := n + 1].\n\t^n"),
                    method("isEmpty", "testing", Side::Instance,
                        "isEmpty\n\t^self size = 0"),
                    method("collect:", "enumerating", Side::Instance,
                        "collect: aBlock\n\t| result |\n\tresult := OrderedCollection new.\n\tself do: [:each | result add: (aBlock value: each)].\n\t^result"),
                ],
            },
            MockClass {
                name: "OrderedCollection".into(),
                superclass: Some("Collection".into()),
                package: "Collections".into(),
                comment: "A growable, ordered sequence of elements.".into(),
                instance_vars: "".into(),
                class_vars: "".into(),
                methods: vec![
                    method("add:", "adding", Side::Instance,
                        "add: anObject\n\tlastIndex := lastIndex + 1.\n\tself at: lastIndex put: anObject.\n\t^anObject"),
                    method("removeFirst", "removing", Side::Instance,
                        "removeFirst\n\t| first |\n\tfirst := self at: firstIndex.\n\tfirstIndex := firstIndex + 1.\n\t^first"),
                    method("do:", "iterating", Side::Instance,
                        "do: aBlock\n\tfirstIndex to: lastIndex do: [:i | aBlock value: (self at: i)]"),
                    method("new", "instance creation", Side::Class,
                        "new\n\t^self new: 10"),
                ],
            },
            MockClass {
                name: "Dictionary".into(),
                superclass: Some("Collection".into()),
                package: "Collections".into(),
                comment: "A collection of key-value associations.".into(),
                instance_vars: "".into(),
                class_vars: "".into(),
                methods: vec![
                    method("at:put:", "accessing", Side::Instance,
                        "at: key put: aValue\n\tself associationAt: key ifAbsent: [self addAssociation: key -> aValue].\n\t^aValue"),
                    method("at:", "accessing", Side::Instance,
                        "at: key\n\t^self at: key ifAbsent: [self error: 'key not found']"),
                    method("do:", "iterating", Side::Instance,
                        "do: aBlock\n\tself associationsDo: [:assoc | aBlock value: assoc value]"),
                ],
            },
            MockClass {
                name: "Visual".into(),
                superclass: Some("Object".into()),
                package: "GUI".into(),
                comment: "An on-screen visual element: has a parent, a position, and an allocation.".into(),
                instance_vars: "".into(),
                class_vars: "".into(),
                methods: vec![
                    method("position", "accessing", Side::Instance,
                        "position\n\t^position"),
                    method("position:", "accessing", Side::Instance,
                        "position: aPoint\n\tposition := aPoint"),
                    method("parent:", "accessing", Side::Instance,
                        "parent: aVisual\n\tparent := aVisual"),
                ],
            },
            MockClass {
                name: "Button".into(),
                superclass: Some("Visual".into()),
                package: "GUI".into(),
                comment: "A clickable button showing a label or image, running an action block when pressed.".into(),
                instance_vars: "".into(),
                class_vars: "".into(),
                methods: vec![
                    method("label:", "accessing", Side::Instance,
                        "label: aString\n\tlabel := aString"),
                    method("action:", "accessing", Side::Instance,
                        "action: aBlock\n\taction := aBlock"),
                    method("click", "events", Side::Instance,
                        "click\n\taction value: self"),
                    method("labeled:action:", "instance creation", Side::Class,
                        "labeled: aString action: aBlock\n\t^self new label: aString; action: aBlock; yourself"),
                ],
            },
            MockClass {
                name: "Application".into(),
                superclass: Some("Object".into()),
                package: "GUI".into(),
                comment: "Abstract superclass for tools/widgets that answer a Visual via imbeddedVisual.".into(),
                instance_vars: "".into(),
                class_vars: "".into(),
                methods: vec![
                    method("imbeddedVisual", "visuals", Side::Instance,
                        "imbeddedVisual\n\t^self buildVisualTop: false"),
                    method("buildVisualTop:", "visuals", Side::Instance,
                        "buildVisualTop: forTop\n\tself subclassResponsibility"),
                ],
            },
        ];
        Self { classes }
    }

    pub fn packages(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for c in &self.classes {
            if !seen.contains(&c.package) {
                seen.push(c.package.clone());
            }
        }
        seen
    }

    pub fn classes_in_package(&self, package: &str) -> Vec<&MockClass> {
        self.classes.iter().filter(|c| c.package == package).collect()
    }

    pub fn class_named(&self, name: &str) -> Option<&MockClass> {
        self.classes.iter().find(|c| c.name == name)
    }

    /// Convenience wrapper matching `image_store::Image::class_comment`'s
    /// shape — `browser_render.rs` calls this name uniformly regardless of
    /// which backing store (`gui/src/vm_host.rs`) built the `MockWorld` it's
    /// rendering from.
    pub fn class_comment(&self, name: &str) -> Option<String> {
        self.class_named(name).map(|c| c.comment.clone())
    }

    pub fn subclasses_of(&self, name: &str) -> Vec<&MockClass> {
        self.classes.iter().filter(|c| c.superclass.as_deref() == Some(name)).collect()
    }

    /// Root classes of a package's hierarchy view — a class in the package
    /// whose superclass either doesn't exist or lives in a different
    /// package (so e.g. `OrderedCollection`'s tree in "Collections" starts
    /// at `Collection`, but `Collection` itself doesn't re-root at `Object`
    /// since `Object` lives in "Kernel").
    pub fn package_roots(&self, package: &str) -> Vec<&MockClass> {
        self.classes_in_package(package)
            .into_iter()
            .filter(|c| match &c.superclass {
                None => true,
                Some(super_name) => self.class_named(super_name).map(|s| s.package != package).unwrap_or(true),
            })
            .collect()
    }

    pub fn categories(&self, class_name: &str, side: Side) -> Vec<String> {
        self.class_named(class_name).map(|c| c.categories(side)).unwrap_or_default()
    }

    pub fn methods_in(&self, class_name: &str, side: Side, category: &str) -> Vec<MockMethod> {
        self.class_named(class_name)
            .map(|c| c.methods_in(side, category).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Every method on `class_name`'s `side`, regardless of category — the
    /// "(all)" view the methods pane defaults to (`browser_render.rs`) so a
    /// freshly created, still-methodless class (no categories exist yet to
    /// pick from) isn't stuck showing nothing, and so browsing doesn't
    /// require picking a category first when everything's still sitting in
    /// one anyway.
    pub fn methods_of(&self, class_name: &str, side: Side) -> Vec<MockMethod> {
        self.class_named(class_name).map(|c| c.methods_of(side).into_iter().cloned().collect()).unwrap_or_default()
    }

    pub fn method_source(&self, class_name: &str, side: Side, selector: &str) -> Option<String> {
        self.class_named(class_name)?.methods.iter().find(|m| m.side == side && m.selector == selector).map(|m| m.source.clone())
    }

    pub fn class_named_mut(&mut self, name: &str) -> Option<&mut MockClass> {
        self.classes.iter_mut().find(|c| c.name == name)
    }

    /// "Accept" a method's edited source (stand-in for `mirror
    /// addMethod:category:ifFail:`, `../../docs/APPS.md` §5.2 — no real
    /// parsing/compilation here, this mock just replaces the text). Returns
    /// `false` if `class_name`/`selector` don't name an existing method,
    /// so the caller can distinguish "saved" from "nothing to save".
    pub fn set_method_source(&mut self, class_name: &str, side: Side, selector: &str, new_source: String) -> bool {
        let Some(class) = self.class_named_mut(class_name) else { return false };
        let Some(m) = class.methods.iter_mut().find(|m| m.side == side && m.selector == selector) else { return false };
        m.source = new_source;
        true
    }

    /// "Accept" an edited class comment.
    pub fn set_class_comment(&mut self, class_name: &str, new_comment: String) -> bool {
        let Some(class) = self.class_named_mut(class_name) else { return false };
        class.comment = new_comment;
        true
    }
}

impl Default for MockWorld {
    fn default() -> Self {
        Self::seed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_has_multiple_packages() {
        let world = MockWorld::seed();
        let packages = world.packages();
        assert!(packages.contains(&"Kernel".to_string()));
        assert!(packages.contains(&"Collections".to_string()));
        assert!(packages.contains(&"GUI".to_string()));
    }

    #[test]
    fn subclasses_of_object_include_collection_and_visual() {
        let world = MockWorld::seed();
        let names: Vec<_> = world.subclasses_of("Object").iter().map(|c| c.name.clone()).collect();
        assert!(names.contains(&"Collection".to_string()));
        assert!(names.contains(&"Visual".to_string()));
        assert!(names.contains(&"Application".to_string()));
    }

    #[test]
    fn package_roots_does_not_reach_into_other_packages() {
        let world = MockWorld::seed();
        let roots: Vec<_> = world.package_roots("Collections").iter().map(|c| c.name.clone()).collect();
        // Collection has no in-package superclass (Object lives in Kernel),
        // so it's a root here even though it isn't a hierarchy root overall.
        assert!(roots.contains(&"Collection".to_string()));
        assert!(!roots.contains(&"OrderedCollection".to_string()));
    }

    #[test]
    fn categories_and_methods_round_trip() {
        let world = MockWorld::seed();
        let cats = world.categories("Collection", Side::Instance);
        assert!(cats.contains(&"iterating".to_string()));
        let methods = world.methods_in("Collection", Side::Instance, "iterating");
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].selector, "do:");
    }

    #[test]
    fn methods_of_ignores_category_and_includes_a_fresh_classs_empty_list() {
        let world = MockWorld::seed();
        let all = world.methods_of("Collection", Side::Instance);
        assert_eq!(all.len(), 4); // do:, size, isEmpty, collect: across every category

        let mut fresh = MockWorld::empty();
        fresh.add_class("Empty", None, "Test", "", "", "");
        assert!(fresh.methods_of("Empty", Side::Instance).is_empty());
    }

    #[test]
    fn method_source_is_found_by_class_and_selector() {
        let world = MockWorld::seed();
        let src = world.method_source("OrderedCollection", Side::Instance, "add:").unwrap();
        assert!(src.starts_with("add: anObject"));
    }

    #[test]
    fn class_side_methods_are_distinct_from_instance_side() {
        let world = MockWorld::seed();
        let class_side = world.categories("Object", Side::Class);
        assert_eq!(class_side, vec!["instance creation".to_string()]);
        let instance_side = world.categories("Object", Side::Instance);
        assert!(!instance_side.contains(&"instance creation".to_string()));
    }

    #[test]
    fn set_method_source_updates_and_reports_success() {
        let mut world = MockWorld::seed();
        let ok = world.set_method_source("Object", Side::Instance, "hash", "hash\n\t^42".into());
        assert!(ok);
        assert_eq!(world.method_source("Object", Side::Instance, "hash").unwrap(), "hash\n\t^42");
    }

    #[test]
    fn set_method_source_reports_failure_for_unknown_selector() {
        let mut world = MockWorld::seed();
        let ok = world.set_method_source("Object", Side::Instance, "nope", "x".into());
        assert!(!ok);
    }

    #[test]
    fn set_class_comment_updates_and_reports_success() {
        let mut world = MockWorld::seed();
        let ok = world.set_class_comment("Object", "A new comment.".into());
        assert!(ok);
        assert_eq!(world.class_named("Object").unwrap().comment, "A new comment.");
    }

    #[test]
    fn remove_class_removes_it_and_leaves_subclass_superclass_string_untouched() {
        let mut world = MockWorld::seed();
        assert!(world.remove_class("Object"));
        assert!(world.class_named("Object").is_none());
        assert!(!world.packages().contains(&"Kernel".to_string()), "Kernel had only Object");
        // Collection's superclass string field still literally says
        // "Object" — package_roots treats that as absent, same as image_store.
        let roots = world.package_roots("Collections");
        assert!(roots.iter().any(|c| c.name == "Collection"));
        assert!(!world.remove_class("Object")); // already gone
        assert!(!world.remove_class("Nonexistent"));
    }

    #[test]
    fn remove_method_removes_it_only_from_its_own_class() {
        let mut world = MockWorld::seed();
        assert!(world.remove_method("Object", Side::Instance, "hash"));
        assert_eq!(world.method_source("Object", Side::Instance, "hash"), None);
        assert!(world.method_source("Object", Side::Instance, "printString").is_some());
        assert!(!world.remove_method("Object", Side::Instance, "hash")); // already gone
        assert!(!world.remove_method("Object", Side::Instance, "nope"));
    }

    #[test]
    fn create_or_reopen_class_refuses_a_live_name_but_allows_a_removed_one() {
        let mut world = MockWorld::seed();
        assert_eq!(
            world.create_or_reopen_class("Stream", Some("Object"), "Streams", "A stream.", ""),
            ClassCreateOutcome::Created
        );
        assert_eq!(
            world.create_or_reopen_class("Object", None, "Kernel", "Hijacked!", ""),
            ClassCreateOutcome::AlreadyLive
        );
        assert_eq!(world.class_named("Object").unwrap().comment, "The root of the class hierarchy. Defines default behavior shared by every object.");

        world.remove_class("Collection");
        assert_eq!(
            world.create_or_reopen_class("Collection", Some("Object"), "Collections", "Reborn.", "elements"),
            ClassCreateOutcome::Created
        );
        assert_eq!(world.class_named("Collection").unwrap().comment, "Reborn.");
    }

    #[test]
    fn create_or_reopen_method_creates_and_redefines() {
        let mut world = MockWorld::seed();
        assert!(world.create_or_reopen_method("Object", Side::Instance, "printOn:", "printing", "printOn: s\n\t^s"));
        assert_eq!(world.method_source("Object", Side::Instance, "printOn:").unwrap(), "printOn: s\n\t^s");

        assert!(world.create_or_reopen_method("Object", Side::Instance, "hash", "comparing", "hash\n\t^0"));
        assert_eq!(world.method_source("Object", Side::Instance, "hash").unwrap(), "hash\n\t^0");

        assert!(!world.create_or_reopen_method("Nonexistent", Side::Instance, "foo", "cat", "foo"));
    }

    #[test]
    fn set_class_definition_changes_superclass_and_ivars_only() {
        let mut world = MockWorld::seed();
        assert!(world.set_class_definition("Collection", Some("Object".to_string()), "elements size".to_string()));
        let c = world.class_named("Collection").unwrap();
        assert_eq!(c.superclass.as_deref(), Some("Object"));
        assert_eq!(c.instance_vars, "elements size");
        assert_eq!(c.comment, "Abstract superclass for objects that hold a group of elements.");
        assert!(!world.set_class_definition("Nonexistent", None, String::new()));
    }
}
