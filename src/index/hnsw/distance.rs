/// Defines the distance metrics available for vector comparisons in the HNSW index.
///
/// Implements standard distance measures required by approximate nearest neighbor algorithms.
#[derive(Debug)]
pub enum DistanceMetric {
    Euclidean,
    Cosine,
    DotProduct,
}

impl DistanceMetric {
    /// Computes the exact distance between two vectors according to the selected metric.
    ///
    /// # Parameters
    /// - `a`: The first vector slice to compare.
    /// - `b`: The second vector slice to compare.
    ///
    /// # Invariants
    /// The caller must ensure that slices `a` and `b` have exactly the same length. Panics otherwise.
    pub fn compute(&self, a: &[f64], b: &[f64]) -> f64 {
        assert_eq!(a.len(), b.len(), "vector dimension mismatch");
        match self {
            DistanceMetric::Euclidean => self.compute_squared(a, b).sqrt(),
            DistanceMetric::Cosine => {
                let mut dot = 0.0;
                let mut mag_a = 0.0;
                let mut mag_b = 0.0;
                for i in 0..a.len() {
                    dot += a[i] * b[i];
                    mag_a += a[i] * a[i];
                    mag_b += b[i] * b[i];
                }
                let mag_a = mag_a.sqrt();
                let mag_b = mag_b.sqrt();
                // Zero-magnitude vectors have no meaningful direction;
                // return maximum distance (1.0) rather than dividing by zero.
                if mag_a == 0.0 || mag_b == 0.0 {
                    return 1.0;
                }
                1.0 - (dot / (mag_a * mag_b))
            }
            DistanceMetric::DotProduct => {
                let mut dot = 0.0;
                for i in 0..a.len() {
                    dot += a[i] * b[i];
                }
                -dot
            }
        }
    }

    /// Computes the squared Euclidean distance without calculating the final square root.
    ///
    /// This is used internally to avoid the computational cost of the `sqrt` operation when
    /// relative distance ordering is sufficient.
    ///
    /// # Parameters
    /// - `a`: The first vector slice to compare.
    /// - `b`: The second vector slice to compare.
    ///
    /// # Invariants
    /// The caller must ensure that slices `a` and `b` have exactly the same length. Panics otherwise.
    pub fn compute_squared(&self, a: &[f64], b: &[f64]) -> f64 {
        assert_eq!(a.len(), b.len(), "vector dimension mismatch");
        let mut sum = 0.0;
        for i in 0..a.len() {
            let diff = a[i] - b[i];
            sum += diff * diff;
        }
        sum
    }

    /// Returns the default distance metric for new HNSW graphs.
    ///
    /// Cosine distance is used as the default since it's the standard metric for text embeddings
    /// from language models, the primary use case for venom-db's RAG target workload.
    pub fn default() -> Self {
        DistanceMetric::Cosine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_euclidean_known_values() {
        let d = DistanceMetric::Euclidean;
        let result = d.compute(&[0.0, 0.0], &[3.0, 4.0]);
        assert!((result - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_euclidean_identical_vectors() {
        let d = DistanceMetric::Euclidean;
        let result = d.compute(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]);
        assert!(result.abs() < 1e-10);
    }

    #[test]
    #[should_panic(expected = "vector dimension mismatch")]
    fn test_euclidean_dimension_mismatch() {
        let d = DistanceMetric::Euclidean;
        d.compute(&[1.0, 2.0], &[1.0]);
    }

    #[test]
    fn test_euclidean_squared() {
        let d = DistanceMetric::Euclidean;
        let result = d.compute_squared(&[0.0, 0.0], &[3.0, 4.0]);
        assert!((result - 25.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_orthogonal_vectors() {
        let d = DistanceMetric::Cosine;
        let result = d.compute(&[1.0, 0.0], &[0.0, 1.0]);
        assert!((result - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_identical_vectors() {
        let d = DistanceMetric::Cosine;
        let result = d.compute(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]);
        assert!(result.abs() < 1e-10);
    }

    #[test]
    fn test_cosine_zero_vector() {
        let d = DistanceMetric::Cosine;
        let result = d.compute(&[0.0, 0.0], &[1.0, 2.0]);
        assert!((result - 1.0).abs() < 1e-10);
    }

    #[test]
    #[should_panic(expected = "vector dimension mismatch")]
    fn test_cosine_dimension_mismatch() {
        let d = DistanceMetric::Cosine;
        d.compute(&[1.0], &[1.0, 2.0]);
    }

    #[test]
    fn test_dot_product_known_values() {
        let d = DistanceMetric::DotProduct;
        let result = d.compute(&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]);
        assert!((result - -32.0).abs() < 1e-10);
    }

    #[test]
    fn test_dot_product_identical_unit_vectors() {
        let d = DistanceMetric::DotProduct;
        let result = d.compute(&[1.0, 0.0], &[1.0, 0.0]);
        assert!((result - -1.0).abs() < 1e-10);
    }

    #[test]
    #[should_panic(expected = "vector dimension mismatch")]
    fn test_dot_product_dimension_mismatch() {
        let d = DistanceMetric::DotProduct;
        d.compute(&[1.0, 2.0, 3.0], &[1.0]);
    }

    #[test]
    fn test_default_is_cosine() {
        let d = DistanceMetric::default();
        let result = d.compute(&[1.0, 0.0], &[0.0, 1.0]);
        assert!((result - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_cosine_distance_symmetric() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        use rand::Rng;

        let mut rng = StdRng::seed_from_u64(77);
        let dims = 16;
        let a: Vec<f64> = (0..dims).map(|_| rng.r#gen::<f64>() * 2.0 - 1.0).collect();
        let b: Vec<f64> = (0..dims).map(|_| rng.r#gen::<f64>() * 2.0 - 1.0).collect();

        let metrics = [
            DistanceMetric::Euclidean,
            DistanceMetric::Cosine,
            DistanceMetric::DotProduct,
        ];

        for m in &metrics {
            let ab = m.compute(&a, &b);
            let ba = m.compute(&b, &a);
            assert!(
                (ab - ba).abs() < 1e-10,
                "asymmetry in {:?}: compute(a,b)={} != compute(b,a)={}", m, ab, ba
            );
        }
    }

    #[test]
    fn test_euclidean_triangle_inequality() {
        use rand::SeedableRng;
        use rand::rngs::StdRng;
        use rand::Rng;

        let mut rng = StdRng::seed_from_u64(88);
        let dims = 8;
        let a: Vec<f64> = (0..dims).map(|_| rng.r#gen::<f64>()).collect();
        let b: Vec<f64> = (0..dims).map(|_| rng.r#gen::<f64>()).collect();
        let c: Vec<f64> = (0..dims).map(|_| rng.r#gen::<f64>()).collect();

        let d = DistanceMetric::Euclidean;
        let ac = d.compute(&a, &c);
        let ab = d.compute(&a, &b);
        let bc = d.compute(&b, &c);

        assert!(
            ac <= ab + bc + 1e-10,
            "triangle inequality violated: d(a,c)={} > d(a,b)+d(b,c)={}",
            ac, ab + bc
        );
    }
}
