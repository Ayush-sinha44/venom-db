pub mod distance;

use std::collections::HashSet;
use distance::DistanceMetric;

pub struct HnswConfig {
    pub m: usize,
    pub m_max: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
    pub ml: f64,
}

impl HnswConfig {
    pub fn new(m: usize, ef_construction: usize, ef_search: usize) -> Self {
        Self {
            m,
            m_max: 2 * m,
            ef_construction,
            ef_search,
            ml: 1.0 / (m as f64).ln(),
        }
    }

    pub fn default() -> Self {
        Self::new(16, 200, 50)
    }
}

pub struct Vector {
    pub dims: usize,
    pub data: Vec<f64>,
}

impl Vector {
    pub fn new(data: Vec<f64>) -> Self {
        let dims = data.len();
        Self { dims, data }
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    pub fn as_slice(&self) -> &[f64] {
        &self.data
    }
}

impl PartialEq for Vector {
    fn eq(&self, other: &Self) -> bool {
        self.dims == other.dims && self.data == other.data
    }
}

pub struct HnswNode {
    pub id: u32,
    pub vector: Vector,
    pub layers: Vec<Vec<u32>>,
}

pub struct HnswGraph {
    pub config: HnswConfig,
    pub nodes: Vec<HnswNode>,
    pub entry_point: Option<u32>,
    pub max_layer: usize,
    pub dims: usize,
}

pub fn select_neighbors(candidates: &[(u32, f64)], m: usize) -> Vec<u32> {
    candidates.iter().take(m).map(|(id, _)| *id).collect()
}

impl HnswGraph {
    pub fn new(dims: usize, config: HnswConfig) -> Self {
        Self {
            config,
            nodes: Vec::new(),
            entry_point: None,
            max_layer: 0,
            dims,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get_node(&self, id: u32) -> Option<&HnswNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn search_layer(
        &self,
        query: &Vector,
        entry_points: &[u32],
        ef: usize,
        layer: usize,
        metric: &DistanceMetric,
    ) -> Vec<(u32, f64)> {
        let mut visited: HashSet<u32> = HashSet::new();
        let mut candidates: Vec<(u32, f64)> = Vec::new();
        let mut results: Vec<(u32, f64)> = Vec::new();

        for &ep_id in entry_points {
            if visited.contains(&ep_id) {
                continue;
            }
            if let Some(node) = self.get_node(ep_id) {
                visited.insert(ep_id);
                let dist = metric.compute(query.as_slice(), node.vector.as_slice());
                candidates.push((ep_id, dist));
                results.push((ep_id, dist));
            }
        }

        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        if results.len() > ef {
            results.truncate(ef);
        }

        while !candidates.is_empty() {
            let (current_id, current_dist) = candidates.remove(0);

            let worst_result_dist = results.last().map(|(_, d)| *d).unwrap_or(f64::INFINITY);
            if current_dist > worst_result_dist && results.len() >= ef {
                break;
            }

            if let Some(current_node) = self.get_node(current_id) {
                let neighbors = if layer < current_node.layers.len() {
                    &current_node.layers[layer]
                } else {
                    continue;
                };

                for &neighbor_id in neighbors {
                    if visited.contains(&neighbor_id) {
                        continue;
                    }
                    visited.insert(neighbor_id);

                    if let Some(neighbor_node) = self.get_node(neighbor_id) {
                        let dist = metric.compute(
                            query.as_slice(),
                            neighbor_node.vector.as_slice(),
                        );

                        let worst_result_dist = results.last().map(|(_, d)| *d).unwrap_or(f64::INFINITY);

                        if results.len() < ef || dist < worst_result_dist {
                            results.push((neighbor_id, dist));
                            results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                            if results.len() > ef {
                                results.truncate(ef);
                            }

                            candidates.push((neighbor_id, dist));
                            candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                        }
                    }
                }
            }
        }

        results
    }

    pub fn insert(&mut self, _id: u32, _vector: Vector) {
        todo!("implement on Day 4")
    }

    pub fn search(&self, _query: &Vector, _k: usize) -> Vec<(u32, f64)> {
        todo!("implement on Day 5")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::distance::DistanceMetric;

    fn make_node(id: u32, coords: Vec<f64>, layer0_neighbors: Vec<u32>) -> HnswNode {
        HnswNode {
            id,
            vector: Vector::new(coords),
            layers: vec![layer0_neighbors],
        }
    }

    #[test]
    fn test_config_default_params() {
        let config = HnswConfig::default();
        assert_eq!(config.m, 16);
        assert_eq!(config.m_max, 32);
        assert_eq!(config.ef_construction, 200);
        assert_eq!(config.ef_search, 50);
        assert!((config.ml - (1.0 / (16f64).ln())).abs() < 1e-10);
    }

    #[test]
    fn test_vector_dims() {
        let v = Vector::new(vec![1.0, 2.0, 3.0]);
        assert_eq!(v.dims(), 3);
    }

    #[test]
    fn test_graph_new_is_empty() {
        let config = HnswConfig::default();
        let graph = HnswGraph::new(128, config);
        assert!(graph.is_empty());
        assert_eq!(graph.len(), 0);
        assert!(graph.entry_point.is_none());
        assert_eq!(graph.dims, 128);
    }

    #[test]
    fn test_graph_get_nonexistent_node() {
        let config = HnswConfig::default();
        let graph = HnswGraph::new(3, config);
        assert!(graph.get_node(999).is_none());
    }

    #[test]
    fn test_search_layer_single_node() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        graph.nodes.push(make_node(0, vec![1.0, 1.0], vec![]));
        let query = Vector::new(vec![0.0, 0.0]);
        let metric = DistanceMetric::Euclidean;
        let results = graph.search_layer(&query, &[0], 10, 0, &metric);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn test_search_layer_ordering() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        graph.nodes.push(make_node(0, vec![10.0, 10.0], vec![1, 2]));
        graph.nodes.push(make_node(1, vec![1.0, 1.0], vec![0, 2]));
        graph.nodes.push(make_node(2, vec![5.0, 5.0], vec![0, 1]));
        let query = Vector::new(vec![0.0, 0.0]);
        let metric = DistanceMetric::Euclidean;
        let results = graph.search_layer(&query, &[0], 10, 0, &metric);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, 1);
        assert_eq!(results[1].0, 2);
        assert_eq!(results[2].0, 0);
    }

    #[test]
    fn test_search_layer_ef_limits_results() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        graph.nodes.push(make_node(0, vec![0.0, 0.0], vec![1, 2, 3, 4]));
        graph.nodes.push(make_node(1, vec![1.0, 0.0], vec![0]));
        graph.nodes.push(make_node(2, vec![2.0, 0.0], vec![0]));
        graph.nodes.push(make_node(3, vec![3.0, 0.0], vec![0]));
        graph.nodes.push(make_node(4, vec![4.0, 0.0], vec![0]));
        let query = Vector::new(vec![0.5, 0.0]);
        let metric = DistanceMetric::Euclidean;
        let results = graph.search_layer(&query, &[0], 1, 0, &metric);
        assert_eq!(results.len(), 1);
        let results3 = graph.search_layer(&query, &[0], 3, 0, &metric);
        assert_eq!(results3.len(), 3);
    }

    #[test]
    fn test_search_layer_unreachable_nodes() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        graph.nodes.push(make_node(0, vec![10.0, 10.0], vec![1]));
        graph.nodes.push(make_node(1, vec![9.0, 9.0], vec![0]));
        graph.nodes.push(make_node(2, vec![0.0, 0.0], vec![]));
        let query = Vector::new(vec![0.0, 0.0]);
        let metric = DistanceMetric::Euclidean;
        let results = graph.search_layer(&query, &[0], 10, 0, &metric);
        let ids: Vec<u32> = results.iter().map(|(id, _)| *id).collect();
        assert!(!ids.contains(&2));
    }

    #[test]
    fn test_select_neighbors_returns_closest_m() {
        let candidates = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0), (4, 5.0)];
        let selected = select_neighbors(&candidates, 3);
        assert_eq!(selected.len(), 3);
        assert_eq!(selected, vec![0, 1, 2]);
    }

    #[test]
    fn test_select_neighbors_fewer_than_m() {
        let candidates = vec![(0, 1.0), (1, 2.0)];
        let selected = select_neighbors(&candidates, 5);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected, vec![0, 1]);
    }

    #[test]
    fn test_search_layer_multilayer() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        graph.nodes.push(HnswNode {
            id: 0,
            vector: Vector::new(vec![0.0, 0.0]),
            layers: vec![vec![1, 2], vec![1]],
        });
        graph.nodes.push(HnswNode {
            id: 1,
            vector: Vector::new(vec![3.0, 0.0]),
            layers: vec![vec![0, 2], vec![0]],
        });
        graph.nodes.push(HnswNode {
            id: 2,
            vector: Vector::new(vec![1.0, 0.0]),
            layers: vec![vec![0, 1]],
        });
        let query = Vector::new(vec![0.5, 0.0]);
        let metric = DistanceMetric::Euclidean;
        let layer1_results = graph.search_layer(&query, &[0], 10, 1, &metric);
        let layer1_ids: Vec<u32> = layer1_results.iter().map(|(id, _)| *id).collect();
        assert!(layer1_ids.contains(&0));
        assert!(layer1_ids.contains(&1));
        assert!(!layer1_ids.contains(&2));
    }
}
