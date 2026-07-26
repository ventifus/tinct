//! Dict type inference — `compute_sccs` unit tests.
//!
//! Dict inference is fully implemented in `typecheck_cek::run_typecheck_dict`, called from:
//!   - `AfterDictPassZero` handler in `typecheck_cek::apply_cont`
//!   - `process_document` (top-level dict expressions)
//!   - `run_typecheck` Sequential arm (intermediate dict bodies via CEK machine)

#[cfg(test)]
mod tests {
    use crate::ast::{Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode};
    use crate::test_util::sp;
    use crate::typecheck::typecheck_cek::{compute_sccs, Scc};
    use std::sync::Arc;

    /// Helper: build a zero-origin [`SurfaceNode`] from a [`SurfaceExpression`].
    fn sn(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode::new(
            expr,
            crate::ast::Span::rust_source(file!(), line!()),
        ))
    }

    /// Helper: build a `Spanned<SurfaceEntry>` whose value is a `VarRef` to `ref_name`.
    /// Used to encode a dependency edge: this entry's value references `ref_name`.
    fn entry_ref(ref_name: &str) -> Spanned<SurfaceEntry> {
        sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: ref_name.to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            }),
        })
    }

    /// Helper: build a `Spanned<SurfaceEntry>` whose value is an integer literal (no deps).
    fn entry_lit() -> Spanned<SurfaceEntry> {
        sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::Int(0)),
        })
    }

    /// Helper: build a key_entries list of named, non-alias, static-key entries.
    fn key_entries_for(names: &[&str]) -> Vec<(Option<String>, bool, bool)> {
        names
            .iter()
            .map(|n| (Some(n.to_string()), false, true))
            .collect()
    }

    /// Collect the SCC groups as sorted index sets so tests are order-independent within
    /// a group (Tarjan's exact member ordering is implementation-defined).
    fn scc_index_sets(sccs: &[Scc]) -> Vec<Vec<usize>> {
        let mut result: Vec<Vec<usize>> = sccs
            .iter()
            .map(|scc| {
                let mut v = scc.indices.clone();
                v.sort_unstable();
                v
            })
            .collect();
        result.sort();
        result
    }

    // --- compute_sccs unit tests ---

    /// Empty entries: no SCCs produced.
    #[test]
    fn test_scc_empty_entries() {
        let entries: Vec<Spanned<SurfaceEntry>> = vec![];
        let key_entries: Vec<(Option<String>, bool, bool)> = vec![];
        let sccs = compute_sccs(&entries, &key_entries);
        assert!(sccs.is_empty(), "expected no SCCs for empty input");
    }

    /// Linear chain A→B (A references B): two singletons with B processed before A.
    /// A depends on B, so B's SCC appears first in Tarjan's output (dependencies first).
    #[test]
    fn test_scc_linear_chain() {
        // entries[0] = A (references B at index 1)
        // entries[1] = B (no deps)
        let b_entry = entry_lit();
        let a_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: "b".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            }),
        });
        let entries = vec![a_entry, b_entry];
        let key_entries = key_entries_for(&["a", "b"]);
        let sccs = compute_sccs(&entries, &key_entries);

        // Two singleton SCCs
        assert_eq!(sccs.len(), 2, "expected 2 singleton SCCs for a→b chain");
        let sets = scc_index_sets(&sccs);
        assert!(
            sets.contains(&vec![0]),
            "expected SCC containing index 0 (a)"
        );
        assert!(
            sets.contains(&vec![1]),
            "expected SCC containing index 1 (b)"
        );
    }

    /// Two-node mutual cycle A↔B: both should be in the same SCC.
    #[test]
    fn test_scc_two_node_cycle() {
        // entries[0] = A (references B at index 1)
        // entries[1] = B (references A at index 0)
        let a_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: "b".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            }),
        });
        let b_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::VarRef {
                name: "a".to_string(),
                escaped: false,
                resolution: crate::ast::Resolution::new(),
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
                do_infer_placeholder: false,
            }),
        });
        let entries = vec![a_entry, b_entry];
        let key_entries = key_entries_for(&["a", "b"]);
        let sccs = compute_sccs(&entries, &key_entries);

        assert_eq!(sccs.len(), 1, "expected 1 SCC for mutual cycle a↔b");
        let sets = scc_index_sets(&sccs);
        assert_eq!(sets[0], vec![0, 1], "both nodes must be in the same SCC");
    }

    /// Diamond DAG: A→B, A→C, B→D, C→D.
    /// D has no deps, B and C each depend only on D, A depends on B and C.
    /// Expected: four singletons in dependency-first order (D, then B and C in some order, then A).
    #[test]
    fn test_scc_diamond_dag() {
        // indices: A=0, B=1, C=2, D=3
        let a_entry = sp(SurfaceEntry {
            key: None,
            value: sn(SurfaceExpression::Dict(vec![
                sp(SurfaceEntry {
                    key: None,
                    value: sn(SurfaceExpression::VarRef {
                        name: "b".to_string(),
                        escaped: false,
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: None,
                        do_infer_placeholder: false,
                    }),
                }),
                sp(SurfaceEntry {
                    key: None,
                    value: sn(SurfaceExpression::VarRef {
                        name: "c".to_string(),
                        escaped: false,
                        resolution: crate::ast::Resolution::new(),
                        call_dispatch: crate::ast::CallDispatch::new(),
                        annotation: None,
                        do_infer_placeholder: false,
                    }),
                }),
            ])),
        });
        let b_entry = entry_ref("d");
        let c_entry = entry_ref("d");
        let d_entry = entry_lit();

        let entries = vec![a_entry, b_entry, c_entry, d_entry];
        let key_entries = key_entries_for(&["a", "b", "c", "d"]);
        let sccs = compute_sccs(&entries, &key_entries);

        // Four singleton SCCs (no cycles in a DAG)
        assert_eq!(sccs.len(), 4, "expected 4 singleton SCCs for diamond DAG");

        let sets = scc_index_sets(&sccs);
        // Every node appears exactly once
        assert!(sets.contains(&vec![0]), "a (index 0) must appear");
        assert!(sets.contains(&vec![1]), "b (index 1) must appear");
        assert!(sets.contains(&vec![2]), "c (index 2) must appear");
        assert!(sets.contains(&vec![3]), "d (index 3) must appear");

        // Dependency ordering: d must appear before b and c; b and c must appear before a.
        // Tarjan returns SCCs in reverse topological order (dependencies first).
        // Build output-position map: original_index → position in sccs output
        let mut output_pos = [0usize; 4];
        for (scc_pos, scc) in sccs.iter().enumerate() {
            for &idx in &scc.indices {
                output_pos[idx] = scc_pos;
            }
        }
        // d (3) must come before b (1) and c (2)
        assert!(
            output_pos[3] < output_pos[1],
            "d must be processed before b"
        );
        assert!(
            output_pos[3] < output_pos[2],
            "d must be processed before c"
        );
        // b (1) and c (2) must come before a (0)
        assert!(
            output_pos[1] < output_pos[0],
            "b must be processed before a"
        );
        assert!(
            output_pos[2] < output_pos[0],
            "c must be processed before a"
        );
    }
}
