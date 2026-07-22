use std::collections::HashMap;
use std::fmt;

use petgraph::dot::{Config, Dot};
use petgraph::graph::DiGraph;
use petgraph::visit::EdgeRef;

use crate::error::{Error, Result};
use crate::scenario::{Scenario, StepId};

/// Format for graph visualization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphFormat {
    /// DOT format (for Graphviz)
    Dot,
    /// Mermaid format
    Mermaid,
    /// JSON format
    Json,
}

impl fmt::Display for GraphFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphFormat::Dot => write!(f, "dot"),
            GraphFormat::Mermaid => write!(f, "mermaid"),
            GraphFormat::Json => write!(f, "json"),
        }
    }
}

/// Dependency graph for a scenario
#[derive(Debug, Clone)]
pub struct DependencyGraph {
    /// The scenario this graph represents
    scenario: Scenario,

    /// The graph data structure
    graph: DiGraph<StepId, ()>,
}

impl DependencyGraph {
    /// Create a new dependency graph from a scenario
    pub fn new(scenario: Scenario) -> Result<Self> {
        let mut graph = DiGraph::new();
        let mut node_indices = HashMap::new();

        // Add nodes for all steps
        for step in scenario.get_steps() {
            let node_idx = graph.add_node(step.id.clone());
            node_indices.insert(step.id.clone(), node_idx);
        }

        // Add edges for dependencies
        for step in scenario.get_steps() {
            let to_idx = node_indices.get(&step.id).unwrap();

            for dep_id in &step.dependencies {
                if let Some(from_idx) = node_indices.get(dep_id) {
                    graph.add_edge(*from_idx, *to_idx, ());
                } else {
                    return Err(Error::graph(format!(
                        "Step '{}' depends on non-existent step '{}'",
                        step.id, dep_id
                    )));
                }
            }
        }

        // Check for cycles
        if has_cycles(&graph) {
            return Err(Error::graph("Dependency graph contains cycles".to_string()));
        }

        Ok(Self { scenario, graph })
    }

    /// Visualize the dependency graph
    pub fn visualize(&self, format: GraphFormat) -> Result<String> {
        match format {
            GraphFormat::Dot => self.to_dot(),
            GraphFormat::Mermaid => self.to_mermaid(),
            GraphFormat::Json => self.to_json(),
        }
    }

    /// Convert the graph to DOT format
    fn to_dot(&self) -> Result<String> {
        let dot = Dot::with_config(&self.graph, &[Config::EdgeNoLabel]);
        Ok(format!("{dot:?}"))
    }

    /// Convert the graph to Mermaid format
    fn to_mermaid(&self) -> Result<String> {
        let mut result = String::from("graph TD;\n");

        // Add nodes
        for node_idx in self.graph.node_indices() {
            let step_id = &self.graph[node_idx];
            if let Some(step) = self.scenario.get_step(step_id) {
                result.push_str(&format!(
                    "    {}[\"{}\"]\n",
                    sanitize_mermaid_id(&step.id),
                    escape_mermaid_label(&step.name),
                ));
            }
        }

        // Add edges
        for edge in self.graph.edge_references() {
            let from_id = sanitize_mermaid_id(&self.graph[edge.source()]);
            let to_id = sanitize_mermaid_id(&self.graph[edge.target()]);
            result.push_str(&format!("    {from_id} --> {to_id}\n"));
        }

        Ok(result)
    }

    /// Convert the graph to JSON format
    fn to_json(&self) -> Result<String> {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();

        // Add nodes
        for node_idx in self.graph.node_indices() {
            let step_id = &self.graph[node_idx];
            if let Some(step) = self.scenario.get_step(step_id) {
                let mut redacted_url = step.request.url().clone();
                let _ = redacted_url.set_password(None);

                nodes.push(serde_json::json!({
                    "id": step.id,
                    "name": step.name,
                    "method": step.request.method().to_string(),
                    "url": redacted_url.to_string(),
                }));
            }
        }

        // Add edges
        for edge in self.graph.edge_references() {
            let from_id = &self.graph[edge.source()];
            let to_id = &self.graph[edge.target()];
            edges.push(serde_json::json!({
                "source": from_id,
                "target": to_id,
            }));
        }

        let graph = serde_json::json!({
            "scenario": {
                "id": self.scenario.id,
                "name": self.scenario.name,
            },
            "nodes": nodes,
            "edges": edges,
        });

        Ok(serde_json::to_string_pretty(&graph)?)
    }
}

/// Check if a graph has cycles
fn has_cycles<N, E>(graph: &DiGraph<N, E>) -> bool {
    petgraph::algo::is_cyclic_directed(graph)
}

/// Sanitize a step ID into a Mermaid-safe node identifier.
///
/// Mermaid node IDs must be alphanumeric or underscores; any other character
/// (spaces, dashes, quotes) would break the diagram syntax, so each is replaced
/// with an underscore. The same mapping is applied to both node declarations and
/// edges so references stay consistent.
fn sanitize_mermaid_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Escape a free-form name for use inside a Mermaid `["..."]` node label.
///
/// Double quotes are replaced with the `#quot;` entity Mermaid understands so a
/// name containing `"` cannot break out of the label.
fn escape_mermaid_label(label: &str) -> String {
    label.replace('"', "#quot;")
}

/// Trait for visualizing dependency graphs
pub trait GraphVisualizer: Send + Sync {
    /// Visualize a scenario as a dependency graph
    fn visualize(&self, scenario: &Scenario, format: GraphFormat) -> Result<String>;

    /// Clone this visualizer into a fresh boxed trait object (dyn-clone
    /// pattern).
    ///
    /// This lets [`crate::engine::Engine`], which stores a
    /// `Box<dyn GraphVisualizer>`, preserve a custom visualizer across its own
    /// `Clone` instead of silently substituting the default. Implementors that
    /// carry state should clone it here.
    fn clone_box(&self) -> Box<dyn GraphVisualizer>;
}

/// Default implementation of the graph visualizer
pub struct DefaultGraphVisualizer;

impl DefaultGraphVisualizer {
    /// Create a new default graph visualizer
    pub fn new() -> Self {
        Self
    }
}

impl GraphVisualizer for DefaultGraphVisualizer {
    fn visualize(&self, scenario: &Scenario, format: GraphFormat) -> Result<String> {
        let graph = DependencyGraph::new(scenario.clone())?;
        graph.visualize(format)
    }

    fn clone_box(&self) -> Box<dyn GraphVisualizer> {
        Box::new(DefaultGraphVisualizer::new())
    }
}

impl Default for DefaultGraphVisualizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Factory for creating graph visualizers
pub struct GraphVisualizerFactory;

impl GraphVisualizerFactory {
    /// Create a new default graph visualizer
    pub fn create() -> Box<dyn GraphVisualizer> {
        Box::new(DefaultGraphVisualizer::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Request;
    use crate::scenario::{ScenarioBuilder, StepBuilder};

    fn create_test_scenario() -> Scenario {
        let request1 = Request::get("https://localhost/1").build().unwrap();
        let request2 = Request::get("https://localhost/2").build().unwrap();
        let request3 = Request::get("https://localhost/3").build().unwrap();

        let step1 = StepBuilder::new("step1", "Step 1", request1).build();
        let step2 = StepBuilder::new("step2", "Step 2", request2)
            .dependency("step1")
            .build();
        let step3 = StepBuilder::new("step3", "Step 3", request3)
            .dependency("step2")
            .build();

        ScenarioBuilder::new("scenario1", "Test Scenario")
            .step(step1)
            .step(step2)
            .step(step3)
            .build()
            .unwrap()
    }

    #[test]
    fn test_cycle_detection() {
        let request1 = Request::get("https://localhost/1").build().unwrap();
        let request2 = Request::get("https://localhost/2").build().unwrap();

        let step1 = StepBuilder::new("step1", "Step 1", request1)
            .dependency("step2")
            .build();
        let step2 = StepBuilder::new("step2", "Step 2", request2)
            .dependency("step1")
            .build();

        let scenario = ScenarioBuilder::new("scenario1", "Test Scenario")
            .step(step1)
            .step(step2)
            .build();

        assert!(scenario.is_err());
    }

    #[test]
    fn test_graph_visualization() {
        let scenario = create_test_scenario();
        let graph = DependencyGraph::new(scenario).unwrap();

        let dot = graph.visualize(GraphFormat::Dot).unwrap();
        assert!(dot.contains("digraph"));
        assert!(dot.contains("step1"));
        assert!(dot.contains("step2"));
        assert!(dot.contains("step3"));

        let mermaid = graph.visualize(GraphFormat::Mermaid).unwrap();
        assert!(mermaid.contains("graph TD"));
        assert!(mermaid.contains("step1"));
        assert!(mermaid.contains("step2"));
        assert!(mermaid.contains("step3"));
        assert!(mermaid.contains("step1 --> step2"));
        assert!(mermaid.contains("step2 --> step3"));

        let json = graph.visualize(GraphFormat::Json).unwrap();
        assert!(json.contains("\"scenario\""));
        assert!(json.contains("\"nodes\""));
        assert!(json.contains("\"edges\""));
        assert!(json.contains("\"id\": \"step1\""));
        assert!(json.contains("\"id\": \"step2\""));
        assert!(json.contains("\"id\": \"step3\""));
    }

    #[test]
    fn test_graph_visualizer() {
        let scenario = create_test_scenario();
        let visualizer = DefaultGraphVisualizer::new();

        let dot = visualizer.visualize(&scenario, GraphFormat::Dot).unwrap();
        assert!(dot.contains("digraph"));

        let mermaid = visualizer
            .visualize(&scenario, GraphFormat::Mermaid)
            .unwrap();
        assert!(mermaid.contains("graph TD"));

        let json = visualizer.visualize(&scenario, GraphFormat::Json).unwrap();
        assert!(json.contains("\"scenario\""));
    }

    #[test]
    fn test_mermaid_escaping_and_sanitization() {
        let request = Request::get("https://localhost/1").build().unwrap();
        let step = StepBuilder::new("step-1", "Say \"hi\"", request).build();
        let scenario = ScenarioBuilder::new("scenario1", "Test Scenario")
            .step(step)
            .build()
            .unwrap();
        let graph = DependencyGraph::new(scenario).unwrap();

        let mermaid = graph.visualize(GraphFormat::Mermaid).unwrap();

        // The dash in the ID is sanitized to an underscore for a valid node id.
        assert!(mermaid.contains("step_1[\"Say #quot;hi#quot;\"]"));
        // The raw quote must not survive into the label.
        assert!(!mermaid.contains("Say \"hi\""));
    }

    #[test]
    fn test_sanitize_and_escape_helpers() {
        assert_eq!(sanitize_mermaid_id("step-1 a.b"), "step_1_a_b");
        assert_eq!(sanitize_mermaid_id("keep_this1"), "keep_this1");
        assert_eq!(escape_mermaid_label("a\"b\"c"), "a#quot;b#quot;c");
    }
}
