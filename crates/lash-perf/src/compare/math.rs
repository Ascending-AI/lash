//! The two-point affine cost model; durations and slopes retain nanoseconds.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub(crate) struct Fit {
    pub slope_ns: f64,
    pub intercept_ns: f64,
}

pub(crate) fn fit(n: [u64; 2], t: [f64; 2]) -> Fit {
    let slope_ns = (t[1] - t[0]) / (n[1] - n[0]) as f64;
    Fit {
        slope_ns,
        intercept_ns: t[0] - slope_ns * n[0] as f64,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn slope_removes_synthetic_fixed_execution_cost() {
        let engine = |n: u64| 900_000.0 + 17.0 * n as f64;
        let fit = super::fit([100, 10_000], [engine(100), engine(10_000)]);
        assert_eq!(fit.slope_ns, 17.0);
        assert_eq!(fit.intercept_ns, 900_000.0);
    }
}
