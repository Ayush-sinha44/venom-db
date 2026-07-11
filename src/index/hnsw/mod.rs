pub mod distance;

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
}
