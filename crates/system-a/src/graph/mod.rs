//! Dependency graph construction and topological ordering.
//!
//! Uses `petgraph` to build a directed acyclic graph (DAG) of unit
//! dependencies. Edges represent "must start before" relationships.

use std::collections::HashMap;

use anyhow::{bail, Result};
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use tracing::{debug, warn};

use crate::unit::types::UnitFile;

/// Edge weight indicating how strong the ordering dependency is.
/// Used for cycle-breaking: weak edges (Wants, After) are removed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Strong ordering (Requires, Requisite, BindsTo) — harder to break.
    Strong,
    /// Weak ordering (Wants, After, Before) — preferred for cycle-breaking.
    Weak,
}

/// A resolved dependency graph.
#[derive(Debug)]
pub struct DependencyGraph {
    /// The underlying directed graph. Edges go from dependency to dependent
    /// (i.e., edge A→B means "A must start before B").
    graph: DiGraph<String, EdgeKind>,
    /// Maps unit name → node index.
    index: HashMap<String, NodeIndex>,
}

impl DependencyGraph {
    /// Build a dependency graph from a collection of loaded unit files.
    pub fn build<'a>(units: impl Iterator<Item = &'a UnitFile>) -> Self {
        let mut graph = DiGraph::new();
        let mut index: HashMap<String, NodeIndex> = HashMap::new();

        let units: Vec<&UnitFile> = units.collect();

        // First pass: add all units as nodes.
        for unit in &units {
            let node = graph.add_node(unit.name.clone());
            index.insert(unit.name.clone(), node);
        }

        // Helper: get or create a node for a dependency name.
        let get_or_create_node =
            |graph: &mut DiGraph<String, EdgeKind>,
             index: &mut HashMap<String, NodeIndex>,
             name: &str|
             -> NodeIndex {
                if let Some(&n) = index.get(name) {
                    n
                } else {
                    let n = graph.add_node(name.to_string());
                    index.insert(name.to_string(), n);
                    n
                }
            };

        // Second pass: add dependency edges.
        for unit in &units {
            let Some(&unit_node) = index.get(&unit.name) else {
                continue;
            };

            // `After=X` means X must start before this unit → edge X→unit (Weak).
            for dep in &unit.unit.after {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(dep_node, unit_node) {
                    graph.add_edge(dep_node, unit_node, EdgeKind::Weak);
                }
            }

            // `Before=X` means this unit must start before X → edge unit→X (Weak).
            for dep in &unit.unit.before {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(unit_node, dep_node) {
                    graph.add_edge(unit_node, dep_node, EdgeKind::Weak);
                }
            }

            // `Requisite=X` implies ordering edge X→unit (Strong).
            // At scheduling time, X must already be active (not started for it).
            for dep in &unit.unit.requisite {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(dep_node, unit_node) {
                    graph.add_edge(dep_node, unit_node, EdgeKind::Strong);
                }
            }

            // `BindsTo=X` implies ordering edge X→unit (Strong).
            for dep in &unit.unit.binds_to {
                let dep_node = get_or_create_node(&mut graph, &mut index, dep);
                if !graph.contains_edge(dep_node, unit_node) {
                    graph.add_edge(dep_node, unit_node, EdgeKind::Strong);
                }
            }

            // `Requires=` and `Wants=` are dependency declarations but don't
            // imply ordering by themselves (ordering is via After/Before).
            // However, if combined with After, the edge is already present.
        }

        DependencyGraph { graph, index }
    }

    /// Return a topological ordering of all units.
    ///
    /// Units earlier in the list must be started first. If there is a cycle,
    /// this method attempts to break it by removing weak edges (mimicking
    /// systemd's cycle-breaking behavior). Returns an error only if a cycle
    /// consists entirely of strong edges and cannot be broken.
    pub fn topological_order(&self) -> Result<Vec<String>> {
        // Try the fast path first.
        if let Ok(nodes) = toposort(&self.graph, None) {
            let names = nodes.into_iter().map(|n| self.graph[n].clone()).collect();
            return Ok(names);
        }

        // There is at least one cycle — attempt to break it.
        let mut graph = self.graph.clone();
        let max_attempts = graph.edge_count();

        for _ in 0..max_attempts {
            match toposort(&graph, None) {
                Ok(nodes) => {
                    let names = nodes.into_iter().map(|n| graph[n].clone()).collect();
                    return Ok(names);
                }
                Err(cycle) => {
                    let cycle_node = cycle.node_id();
                    // Find a weak back-edge involving the cycle node and remove it.
                    let weak_edge = graph
                        .edges_directed(cycle_node, petgraph::Direction::Incoming)
                        .find(|e| *e.weight() == EdgeKind::Weak)
                        .map(|e| e.id());

                    if let Some(eid) = weak_edge {
                        let (src, tgt) = graph.edge_endpoints(eid).unwrap();
                        warn!(
                            "Breaking dependency cycle: removing weak edge {} → {}",
                            graph[src], graph[tgt]
                        );
                        graph.remove_edge(eid);
                    } else {
                        // No weak edge to remove — try any incoming edge.
                        let any_edge = graph
                            .edges_directed(cycle_node, petgraph::Direction::Incoming)
                            .next()
                            .map(|e| e.id());
                        if let Some(eid) = any_edge {
                            let (src, tgt) = graph.edge_endpoints(eid).unwrap();
                            warn!(
                                "Breaking dependency cycle: removing strong edge {} → {} (no weak edges available)",
                                graph[src], graph[tgt]
                            );
                            graph.remove_edge(eid);
                        } else {
                            bail!(
                                "Dependency cycle detected involving unit: {} (unable to break)",
                                graph[cycle_node]
                            );
                        }
                    }
                }
            }
        }

        bail!("Unable to resolve dependency cycles after removing {} edges", max_attempts)
    }

    /// Compute the start order for a single unit and all of its transitive
    /// dependencies, respecting ordering constraints.
    ///
    /// Returns unit names in the order they should be started.
    pub fn start_order_for(&self, unit_name: &str) -> Result<Vec<String>> {
        let Some(&start_node) = self.index.get(unit_name) else {
            return Ok(vec![unit_name.to_string()]);
        };

        // Collect all ancestors (nodes that must start before `unit_name`).
        let mut ancestors = vec![];
        let mut stack = vec![start_node];
        let mut visited = std::collections::HashSet::new();

        while let Some(node) = stack.pop() {
            if visited.contains(&node) {
                continue;
            }
            visited.insert(node);
            ancestors.push(node);
            for neighbor in self
                .graph
                .neighbors_directed(node, petgraph::Direction::Incoming)
            {
                stack.push(neighbor);
            }
        }

        // Toposort just the subgraph formed by these ancestors.
        let full_order = self.topological_order()?;
        let ancestor_set: std::collections::HashSet<_> = ancestors.into_iter().collect();
        let ordered: Vec<String> = full_order
            .into_iter()
            .filter(|name| {
                self.index
                    .get(name)
                    .map(|n| ancestor_set.contains(n))
                    .unwrap_or(false)
            })
            .collect();

        debug!(
            "Start order for {}: {:?}",
            unit_name,
            ordered
        );

        Ok(ordered)
    }

    /// Return the `Requires` and `Wants` dependencies of a unit by name.
    pub fn required_deps<'a>(&'a self, unit_name: &str, units: &'a HashMap<String, UnitFile>) -> Vec<String> {
        let Some(unit) = units.get(unit_name) else {
            return vec![];
        };
        unit.unit
            .requires
            .iter()
            .chain(unit.unit.wants.iter())
            .cloned()
            .collect()
    }
}
