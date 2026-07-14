pub enum DistanceMetric {
    Euclidean,
    Cosine,
    DotProduct,
}

impl DistanceMetric {
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

    pub fn compute_squared(&self, a: &[f64], b: &[f64]) -> f64 {
        assert_eq!(a.len(), b.len(), "vector dimension mismatch");
        let mut sum = 0.0;
        for i in 0..a.len() {
            let diff = a[i] - b[i];
            sum += diff * diff;
        }
        sum
    }

    // Cosine is the standard metric for text embeddings from language models,
    // which is the primary use case for venom-db's RAG target workload.
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
}
