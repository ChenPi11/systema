//! Dependency graph construction and topological ordering.
//!
//! Uses `petgraph` to build a directed acyclic graph (DAG) of unit
//! dependencies. Edges represent "must start before" relationships.

use std::collections::HashMap;

use anyhow::{bail, Result};
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use tracing::debug;

use crate::unit::types::UnitFile;

/// A resolved dependency graph.
#[derive(Debug)]
pub struct DependencyGraph {
    /// The underlying directed graph. Edges go from dependency to dependent
    /// (i.e., edge A→B means "A must start before B").
    graph: DiGraph<String, ()>,
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

        // Second pass: add dependency edges.
        for unit in &units {
            let Some(&unit_node) = index.get(&unit.name) else {
                continue;
            };

            // `After=X` means X must start before this unit → edge X→unit.
            for dep in &unit.unit.after {
                if let Some(&dep_node) = index.get(dep) {
                    if !graph.contains_edge(dep_node, unit_node) {
                        graph.add_edge(dep_node, unit_node, ());
                    }
                } else {
                    // Add a placeholder node for unknown units.
                    let dep_node = graph.add_node(dep.clone());
                    index.insert(dep.clone(), dep_node);
                    graph.add_edge(dep_node, unit_node, ());
                }
            }

            // `Before=X` means this unit must start before X → edge unit→X.
            for dep in &unit.unit.before {
                if let Some(&dep_node) = index.get(dep) {
                    if !graph.contains_edge(unit_node, dep_node) {
                        graph.add_edge(unit_node, dep_node, ());
                    }
                } else {
                    let dep_node = graph.add_node(dep.clone());
                    index.insert(dep.clone(), dep_node);
                    graph.add_edge(unit_node, dep_node, ());
                }
            }

            // `Requires=` and `Wants=` are dependency declarations but don't
            // imply ordering by themselves (ordering is via After/Before).
        }

        DependencyGraph { graph, index }
    }

    /// Return a topological ordering of all units.
    ///
    /// Units earlier in the list must be started first. Returns an error if
    /// there is a cycle in the dependency graph.
    pub fn topological_order(&self) -> Result<Vec<String>> {
        match toposort(&self.graph, None) {
            Ok(nodes) => {
                let names = nodes
                    .into_iter()
                    .map(|n| self.graph[n].clone())
                    .collect();
                Ok(names)
            }
            Err(cycle) => {
                let name = &self.graph[cycle.node_id()];
                bail!("Dependency cycle detected involving unit: {}", name)
            }
        }
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
