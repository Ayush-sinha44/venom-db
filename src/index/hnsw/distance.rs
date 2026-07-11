pub enum DistanceMetric {
    Euclidean,
    Cosine,
    DotProduct,
}

impl DistanceMetric {
    pub fn compute(&self, a: &[f64], b: &[f64]) -> f64 {
        assert_eq!(a.len(), b.len(), "vector dimension mismatch");
        match self {
            DistanceMetric::Euclidean => todo!("implement on Day 2"),
            DistanceMetric::Cosine    => todo!("implement on Day 2"),
            DistanceMetric::DotProduct => todo!("implement on Day 2"),
        }
    }
}
