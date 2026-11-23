// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use crate::diagnostics::BuildDiagnostics;
use crate::langtype::ElementType;
use crate::object_tree::*;
use smol_str::{SmolStr, ToSmolStr, format_smolstr};
use std::collections::HashMap;
use std::rc::Rc;

/// This pass make sure that the id of the elements are unique
///
/// It currently does so by adding a number to the existing id. The number counts within the
/// elements of components of one name (anonymous sub-components count with the component that
/// holds them), so an element added to one component renames nothing in the others, and the
/// generated code of the rest of the document stays the same for incremental builds.
/// Anonymous sub-components take the name of the component that holds them in their root's id,
/// since their generated name is made from it and must not collide across components.
pub fn assign_unique_id(doc: &Document) {
    let mut counts = HashMap::<SmolStr, u32>::new();
    doc.visit_all_used_components(|component| {
        if !component.is_global() {
            let count = counts.entry(component.id.clone()).or_default();
            assign_unique_id_in_component(component, count)
        }
    });
    rename_globals(doc, &mut counts);
}

fn assign_unique_id_in_component(component: &Rc<Component>, count: &mut u32) {
    recurse_elem_including_sub_components(component, &(), &mut |elem, _| {
        *count += 1;
        let mut elem_mut = elem.borrow_mut();
        let old_id = if !elem_mut.id.is_empty() {
            elem_mut.id.clone()
        } else {
            elem_mut.base_type.to_smolstr().to_ascii_lowercase().into()
        };
        let enclosing = elem_mut.enclosing_component.upgrade().unwrap();
        let is_root = Rc::ptr_eq(elem, &enclosing.root_element);
        elem_mut.id = if is_root && !Rc::ptr_eq(&enclosing, component) && enclosing.id.is_empty() {
            format_smolstr!("{}-{}-{}", component.id, old_id, count)
        } else {
            format_smolstr!("{}-{}", old_id, count)
        };

        if is_root {
            for o in enclosing.optimized_elements.borrow().iter() {
                *count += 1;
                let mut elem_mut = o.borrow_mut();
                elem_mut.id = format_smolstr!("optimized-{}-{}", elem_mut.id, count);
            }
        }
    });
}

/// Give globals unique name
fn rename_globals(doc: &Document, counts: &mut HashMap<SmolStr, u32>) {
    for g in &doc.used_types.borrow().globals {
        let count = counts.entry(g.id.clone()).or_default();
        *count += 1;
        let mut root = g.root_element.borrow_mut();
        if matches!(&root.base_type, ElementType::Builtin(_)) {
            // builtin global keeps its name
            root.id.clone_from(&g.id);
        } else if let Some(s) = g.exported_global_names.borrow().first() {
            root.id = s.to_smolstr();
        } else if g.from_library.get() {
            root.id = format_smolstr!("{}", g.id);
        } else {
            root.id = format_smolstr!("{}-{}", g.id, count);
        }
    }
}

/// Checks that all ids in the Component are unique
pub fn check_unique_id(doc: &Document, diag: &mut BuildDiagnostics) {
    for component in &doc.inner_components {
        check_unique_id_in_component(component, diag);
    }
}

fn check_unique_id_in_component(component: &Rc<Component>, diag: &mut BuildDiagnostics) {
    struct SeenId {
        element: ElementRc,
        error_reported: bool,
    }
    let mut seen_ids: HashMap<SmolStr, SeenId> = HashMap::new();

    recurse_elem(&component.root_element, &(), &mut |elem, _| {
        let elem_bor = elem.borrow();
        let id = &elem_bor.id;
        if !id.is_empty() {
            if let Some(other_loc) = seen_ids.get_mut(id) {
                debug_assert!(!Rc::ptr_eq(&other_loc.element, elem));
                let message = format!("duplicated element id '{id}'");
                if !other_loc.error_reported {
                    diag.push_error(message.clone(), &*other_loc.element.borrow());
                    other_loc.error_reported = true;
                }
                diag.push_error(message, &*elem_bor);
            } else {
                seen_ids
                    .insert(id.clone(), SeenId { element: elem.clone(), error_reported: false });
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(first_extra: &str) -> Document {
        let mut config =
            crate::CompilerConfiguration::new(crate::generator::OutputFormat::Interpreter);
        config.inline_all_elements = false;
        let mut diag = BuildDiagnostics::default();
        let source = format!(
            r#"
component First {{ Rectangle {{ {first_extra} for x in 2: Text {{}} }} }}
component Second {{ Rectangle {{ for x in 2: Text {{}} }} }}
export component App {{ First {{}} First {{}} Second {{}} Second {{}} }}
"#
        );
        let node = crate::parser::parse(source, Some(std::path::Path::new("t.slint")), &mut diag);
        let (doc, diag, _) = spin_on::spin_on(crate::compile_syntax_node(node, diag, config));
        assert!(!diag.has_errors(), "{:?}", diag.to_string_vec());
        doc
    }

    fn ids_of(doc: &Document, name: &str) -> Vec<SmolStr> {
        let types = doc.used_types.borrow();
        let component = types.sub_components.iter().find(|c| c.id == name).unwrap();
        let mut ids = vec![];
        recurse_elem_including_sub_components(component, &(), &mut |e, _| {
            ids.push(e.borrow().id.clone())
        });
        ids
    }

    #[test]
    fn an_element_added_to_one_component_renames_nothing_in_another() {
        let (before, after) = (compile(""), compile("Rectangle {}"));
        assert_eq!(ids_of(&before, "Second"), ids_of(&after, "Second"));
        assert_ne!(ids_of(&before, "First"), ids_of(&after, "First"));
    }

    #[test]
    fn repeated_components_of_two_components_keep_distinct_names() {
        let doc = compile("");
        let (first, second) = (ids_of(&doc, "First"), ids_of(&doc, "Second"));
        assert!(first.iter().any(|id| id.starts_with("First-")), "{first:?}");
        assert!(second.iter().any(|id| id.starts_with("Second-")), "{second:?}");
    }

    /// The generated Rust of a component is the same when another component changes, so an
    /// incremental build recompiles only the changed one's module.
    #[cfg(feature = "rust")]
    #[test]
    fn an_element_added_to_one_component_leaves_another_s_rust_module_unchanged() {
        let config = crate::CompilerConfiguration::new(crate::generator::OutputFormat::Rust);
        let rust = |doc: &Document| {
            let code = crate::generator::rust::generate(doc, &config).unwrap().to_string();
            let start = code.find("mod m_InnerSecond").expect("Second has a module of its own");
            let end = code[start + 1..].find("mod m_").map_or(code.len(), |e| start + 1 + e);
            code[start..end].to_owned()
        };
        assert_eq!(rust(&compile("")), rust(&compile("Rectangle {}")));
    }
}
