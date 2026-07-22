pub mod distance;

use std::collections::HashSet;
use distance::DistanceMetric;
use rand::Rng;

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

impl Clone for Vector {
    fn clone(&self) -> Self {
        Self {
            dims: self.dims,
            data: self.data.clone(),
        }
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

    fn get_node_mut(&mut self, id: u32) -> Option<&mut HnswNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    fn random_level(&self) -> usize {
        let mut rng = rand::thread_rng();
        let r: f64 = rng.r#gen();
        (-r.ln() * self.config.ml).floor() as usize
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

    pub fn insert(&mut self, id: u32, vector: Vector, metric: &DistanceMetric) {
        let node_level = self.random_level();

        if self.nodes.is_empty() {
            let layers = (0..=node_level).map(|_| Vec::new()).collect();
            self.nodes.push(HnswNode { id, vector, layers });
            self.entry_point = Some(id);
            self.max_layer = node_level;
            return;
        }

        let mut ep_id = self.entry_point.unwrap();
        let query_vec = vector.clone();

        for layer in (node_level + 1..=self.max_layer).rev() {
            let results = self.search_layer(&query_vec, &[ep_id], 1, layer, metric);
            if !results.is_empty() {
                ep_id = results[0].0;
            }
        }

        let new_node_layers: Vec<Vec<u32>> = (0..=node_level).map(|_| Vec::new()).collect();
        self.nodes.push(HnswNode { id, vector, layers: new_node_layers });

        let top_connect = if node_level < self.max_layer { node_level } else { self.max_layer };

        for layer in (0..=top_connect).rev() {
            let candidates = self.search_layer(
                &query_vec,
                &[ep_id],
                self.config.ef_construction,
                layer,
                metric,
            );

            let m_limit = if layer == 0 { self.config.m_max } else { self.config.m };
            let neighbors = select_neighbors(&candidates, m_limit);

            if let Some(new_node) = self.get_node_mut(id) {
                new_node.layers[layer] = neighbors.clone();
            }

            for &neighbor_id in &neighbors {
                if let Some(neighbor) = self.get_node_mut(neighbor_id) {
                    if layer < neighbor.layers.len() {
                        if !neighbor.layers[layer].contains(&id) {
                            neighbor.layers[layer].push(id);
                        }
                    }
                }
            }

            for &neighbor_id in &neighbors {
                let needs_prune = {
                    if let Some(neighbor) = self.get_node(neighbor_id) {
                        if layer < neighbor.layers.len() {
                            neighbor.layers[layer].len() > m_limit
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if needs_prune {
                    let (pruned, old_neighbors) = {
                        let neighbor = self.get_node(neighbor_id).unwrap();
                        let neighbor_vec_data = neighbor.vector.data.clone();
                        let old_list = neighbor.layers[layer].clone();
                        let mut dists: Vec<(u32, f64)> = old_list
                            .iter()
                            .filter_map(|&nid| {
                                self.get_node(nid).map(|n| {
                                    (nid, metric.compute(&neighbor_vec_data, n.vector.as_slice()))
                                })
                            })
                            .collect();
                        dists.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                        (select_neighbors(&dists, m_limit), old_list)
                    };

                    let pruned_set: HashSet<u32> = pruned.iter().copied().collect();
                    let dropped: Vec<u32> = old_neighbors.into_iter()
                        .filter(|nid| !pruned_set.contains(nid))
                        .collect();

                    if let Some(neighbor) = self.get_node_mut(neighbor_id) {
                        neighbor.layers[layer] = pruned;
                    }

                    for dropped_id in dropped {
                        if let Some(dropped_node) = self.get_node_mut(dropped_id) {
                            if layer < dropped_node.layers.len() {
                                dropped_node.layers[layer].retain(|&nid| nid != neighbor_id);
                            }
                        }
                    }
                }
            }

            if !candidates.is_empty() {
                ep_id = candidates[0].0;
            }
        }

        if node_level > self.max_layer {
            self.entry_point = Some(id);
            self.max_layer = node_level;
        }
    }

    pub fn search(&self, query: &Vector, k: usize, metric: &DistanceMetric) -> Vec<(u32, f64)> {
        if self.nodes.is_empty() {
            return Vec::new();
        }

        let mut ep_id = self.entry_point.unwrap();

        for layer in (1..=self.max_layer).rev() {
            let results = self.search_layer(query, &[ep_id], 1, layer, metric);
            if !results.is_empty() {
                ep_id = results[0].0;
            }
        }

        let mut results = self.search_layer(
            query,
            &[ep_id],
            self.config.ef_search.max(k),
            0,
            metric,
        );

        results.truncate(k);
        results
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

    #[test]
    fn test_insert_first_sets_entry_point() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        graph.insert(0, Vector::new(vec![1.0, 2.0]), &metric);
        assert_eq!(graph.len(), 1);
        assert!(graph.entry_point.is_some());
        assert!(graph.get_node(0).is_some());
    }

    #[test]
    fn test_insert_10_all_retrievable() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        for i in 0..10 {
            graph.insert(i, Vector::new(vec![i as f64, 0.0]), &metric);
        }
        assert_eq!(graph.len(), 10);
        for i in 0..10 {
            assert!(graph.get_node(i).is_some());
        }
    }

    #[test]
    fn test_insert_neighbor_lists_bounded() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(3, config);
        let metric = DistanceMetric::Euclidean;
        for i in 0..50 {
            graph.insert(i, Vector::new(vec![i as f64, 0.0, 0.0]), &metric);
        }
        for node in &graph.nodes {
            for (layer_idx, neighbors) in node.layers.iter().enumerate() {
                let limit = if layer_idx == 0 { graph.config.m_max } else { graph.config.m };
                assert!(
                    neighbors.len() <= limit,
                    "node {} layer {} has {} neighbors, limit {}",
                    node.id, layer_idx, neighbors.len(), limit
                );
            }
        }
    }

    #[test]
    fn test_insert_bidirectional() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        for i in 0..20 {
            graph.insert(i, Vector::new(vec![i as f64, 0.0]), &metric);
        }
        for node in &graph.nodes {
            for (layer_idx, neighbors) in node.layers.iter().enumerate() {
                for &neighbor_id in neighbors {
                    let neighbor = graph.get_node(neighbor_id).unwrap();
                    assert!(
                        layer_idx < neighbor.layers.len() && neighbor.layers[layer_idx].contains(&node.id),
                        "node {} -> {} on layer {} but not reverse",
                        node.id, neighbor_id, layer_idx
                    );
                }
            }
        }
    }

    #[test]
    fn test_insert_max_layer_monotonic() {
        let config = HnswConfig::new(4, 16, 10);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        let mut prev_max = 0;
        for i in 0..100 {
            graph.insert(i, Vector::new(vec![i as f64, 0.0]), &metric);
            assert!(graph.max_layer >= prev_max);
            prev_max = graph.max_layer;
        }
    }

    #[test]
    fn test_insert_100_has_multiple_layers() {
        let config = HnswConfig::default();
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        for i in 0..100 {
            graph.insert(i, Vector::new(vec![i as f64, 0.0]), &metric);
        }
        assert!(graph.max_layer >= 1, "100 inserts with M=16 should produce max_layer >= 1");
    }

    #[test]
    fn test_search_empty_graph() {
        let config = HnswConfig::default();
        let graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        let results = graph.search(&Vector::new(vec![1.0, 2.0]), 5, &metric);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_k1_closest() {
        let config = HnswConfig::new(4, 50, 50);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        let points = vec![
            vec![0.0, 0.0],
            vec![10.0, 10.0],
            vec![1.0, 0.0],
            vec![20.0, 20.0],
            vec![0.5, 0.5],
        ];
        for (i, p) in points.iter().enumerate() {
            graph.insert(i as u32, Vector::new(p.clone()), &metric);
        }
        let query = Vector::new(vec![0.0, 0.0]);
        let results = graph.search(&query, 1, &metric);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn test_search_k_larger_than_graph() {
        let config = HnswConfig::new(4, 50, 50);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        for i in 0..3 {
            graph.insert(i, Vector::new(vec![i as f64, 0.0]), &metric);
        }
        let results = graph.search(&Vector::new(vec![0.0, 0.0]), 100, &metric);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_search_ordering() {
        let config = HnswConfig::new(4, 50, 50);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        graph.insert(0, Vector::new(vec![10.0, 0.0]), &metric);
        graph.insert(1, Vector::new(vec![1.0, 0.0]), &metric);
        graph.insert(2, Vector::new(vec![5.0, 0.0]), &metric);
        let results = graph.search(&Vector::new(vec![0.0, 0.0]), 3, &metric);
        assert_eq!(results[0].0, 1);
        assert_eq!(results[1].0, 2);
        assert_eq!(results[2].0, 0);
    }

    #[test]
    fn test_search_single_node() {
        let config = HnswConfig::new(4, 50, 50);
        let mut graph = HnswGraph::new(2, config);
        let metric = DistanceMetric::Euclidean;
        graph.insert(42, Vector::new(vec![99.0, 99.0]), &metric);
        let r1 = graph.search(&Vector::new(vec![0.0, 0.0]), 1, &metric);
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].0, 42);
        let r2 = graph.search(&Vector::new(vec![1000.0, 1000.0]), 1, &metric);
        assert_eq!(r2[0].0, 42);
    }

    #[test]
    fn test_recall_500_nodes() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        use rand::Rng;

        let mut rng = StdRng::seed_from_u64(12345);
        let config = HnswConfig::default();
        let metric = DistanceMetric::Euclidean;
        let dims = 16;
        let n = 500;
        let num_queries = 10;
        let k = 10;

        let mut vectors: Vec<Vec<f64>> = Vec::new();
        let mut graph = HnswGraph::new(dims, config);

        for i in 0..n {
            let data: Vec<f64> = (0..dims).map(|_| rng.r#gen::<f64>()).collect();
            vectors.push(data.clone());
            graph.insert(i as u32, Vector::new(data), &metric);
        }

        let mut total_recall = 0.0;

        for _ in 0..num_queries {
            let query_data: Vec<f64> = (0..dims).map(|_| rng.r#gen::<f64>()).collect();
            let query = Vector::new(query_data.clone());

            let mut brute_force: Vec<(u32, f64)> = vectors
                .iter()
                .enumerate()
                .map(|(i, v)| (i as u32, metric.compute(&query_data, v)))
                .collect();
            brute_force.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let true_top_k: HashSet<u32> = brute_force.iter().take(k).map(|(id, _)| *id).collect();

            let hnsw_results = graph.search(&query, k, &metric);
            let hnsw_top_k: HashSet<u32> = hnsw_results.iter().map(|(id, _)| *id).collect();

            let hits = true_top_k.intersection(&hnsw_top_k).count();
            total_recall += hits as f64 / k as f64;
        }

        let avg_recall = total_recall / num_queries as f64;
        assert!(
            avg_recall >= 0.90,
            "recall {:.2}% is below 90% threshold",
            avg_recall * 100.0
        );
    }
    #[test]
    fn test_recall_euclidean() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;

        let mut build_rng = StdRng::seed_from_u64(42);
        let mut query_rng = StdRng::seed_from_u64(99);
        let config = HnswConfig::default();
        let metric = DistanceMetric::Euclidean;
        let dims = 32;
        let n = 500;
        let k = 10;

        let mut vectors: Vec<Vec<f64>> = Vec::new();
        let mut graph = HnswGraph::new(dims, config);

        for i in 0..n {
            let data: Vec<f64> = (0..dims).map(|_| build_rng.r#gen::<f64>()).collect();
            vectors.push(data.clone());
            graph.insert(i as u32, Vector::new(data), &metric);
        }

        let mut total_recall = 0.0;
        for _ in 0..10 {
            let qd: Vec<f64> = (0..dims).map(|_| query_rng.r#gen::<f64>()).collect();
            let query = Vector::new(qd.clone());

            let mut bf: Vec<(u32, f64)> = vectors.iter().enumerate()
                .map(|(i, v)| (i as u32, metric.compute(&qd, v))).collect();
            bf.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let truth: HashSet<u32> = bf.iter().take(k).map(|(id, _)| *id).collect();

            let hnsw: HashSet<u32> = graph.search(&query, k, &metric)
                .iter().map(|(id, _)| *id).collect();
            total_recall += truth.intersection(&hnsw).count() as f64 / k as f64;
        }
        let avg = total_recall / 10.0;
        assert!(avg >= 0.90, "euclidean recall {:.1}% < 90%", avg * 100.0);
    }

    #[test]
    fn test_recall_high_dim() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;

        let mut build_rng = StdRng::seed_from_u64(42);
        let mut query_rng = StdRng::seed_from_u64(99);
        let config = HnswConfig::default();
        let metric = DistanceMetric::Cosine;
        let dims = 128;
        let n = 300;
        let k = 10;

        let mut vectors: Vec<Vec<f64>> = Vec::new();
        let mut graph = HnswGraph::new(dims, config);

        for i in 0..n {
            let data: Vec<f64> = (0..dims).map(|_| build_rng.r#gen::<f64>()).collect();
            vectors.push(data.clone());
            graph.insert(i as u32, Vector::new(data), &metric);
        }

        let mut total_recall = 0.0;
        for _ in 0..10 {
            let qd: Vec<f64> = (0..dims).map(|_| query_rng.r#gen::<f64>()).collect();
            let query = Vector::new(qd.clone());

            let mut bf: Vec<(u32, f64)> = vectors.iter().enumerate()
                .map(|(i, v)| (i as u32, metric.compute(&qd, v))).collect();
            bf.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let truth: HashSet<u32> = bf.iter().take(k).map(|(id, _)| *id).collect();

            let hnsw: HashSet<u32> = graph.search(&query, k, &metric)
                .iter().map(|(id, _)| *id).collect();
            total_recall += truth.intersection(&hnsw).count() as f64 / k as f64;
        }
        let avg = total_recall / 10.0;
        assert!(avg >= 0.85, "high-dim cosine recall {:.1}% < 85%", avg * 100.0);
    }

    #[test]
    fn test_recall_small_dataset() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;

        let mut build_rng = StdRng::seed_from_u64(42);
        let mut query_rng = StdRng::seed_from_u64(99);
        let config = HnswConfig::default();
        let metric = DistanceMetric::Euclidean;
        let dims = 8;
        let n = 50;
        let k = 5;

        let mut vectors: Vec<Vec<f64>> = Vec::new();
        let mut graph = HnswGraph::new(dims, config);

        for i in 0..n {
            let data: Vec<f64> = (0..dims).map(|_| build_rng.r#gen::<f64>()).collect();
            vectors.push(data.clone());
            graph.insert(i as u32, Vector::new(data), &metric);
        }

        let mut total_recall = 0.0;
        for _ in 0..10 {
            let qd: Vec<f64> = (0..dims).map(|_| query_rng.r#gen::<f64>()).collect();
            let query = Vector::new(qd.clone());

            let mut bf: Vec<(u32, f64)> = vectors.iter().enumerate()
                .map(|(i, v)| (i as u32, metric.compute(&qd, v))).collect();
            bf.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let truth: HashSet<u32> = bf.iter().take(k).map(|(id, _)| *id).collect();

            let hnsw: HashSet<u32> = graph.search(&query, k, &metric)
                .iter().map(|(id, _)| *id).collect();
            total_recall += truth.intersection(&hnsw).count() as f64 / k as f64;
        }
        let avg = total_recall / 10.0;
        assert!((avg - 1.0).abs() < 1e-10, "small dataset recall {:.1}% != 100%", avg * 100.0);
    }
}

