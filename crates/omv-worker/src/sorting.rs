//! Slice ordering for stack series (design §4.1).
//!
//! The playback order of a CT/MR stack must follow anatomy, which means the
//! DICOM geometry: each slice's ImagePositionPatient projected onto the
//! series normal (ImageOrientationPatient row x column). InstanceNumber is
//! only a fallback — scanners and PACS routers reorder, renumber, and
//! interleave it in the wild, which is the classic shuffled-stack bug.

use tracing::warn;

#[derive(Debug, Clone)]
pub struct SliceRef {
    pub id: String,
    pub instance_number: i64,
    /// ImagePositionPatient (x, y, z) in mm, when present.
    pub position: Option<[f64; 3]>,
    pub frames: u32,
}

/// Parses a DICOM multi-value numeric string like "-12.5\\0\\3.25".
pub fn parse_floats(s: &str) -> Option<Vec<f64>> {
    s.split('\\')
        .map(|p| p.trim().parse::<f64>().ok())
        .collect::<Option<Vec<f64>>>()
}

pub fn parse_position(s: &str) -> Option<[f64; 3]> {
    match parse_floats(s)?.as_slice() {
        [x, y, z] => Some([*x, *y, *z]),
        _ => None,
    }
}

/// Parses ImageOrientationPatient ("rx\\ry\\rz\\cx\\cy\\cz") into the row
/// and column direction cosines.
pub fn parse_orientation(s: &str) -> Option<[[f64; 3]; 2]> {
    match parse_floats(s)?.as_slice() {
        [a, b, c, d, e, f] => Some([[*a, *b, *c], [*d, *e, *f]]),
        _ => None,
    }
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Sorts slices into playback order. Returns the method used, for logging
/// and for the conversion QA trail: "geometric" or "instance-number".
///
/// Geometric ordering applies when every slice has a position and the series
/// has a valid orientation; it is abandoned (with a warning) if projections
/// collapse onto each other (duplicate positions — e.g. a multi-phase series
/// mixed into one stack), where geometry alone cannot define an order.
pub fn sort_slices(orientation: Option<[[f64; 3]; 2]>, slices: &mut [SliceRef]) -> &'static str {
    if slices.len() > 1 {
        if let Some([row, col]) = orientation {
            let normal = cross(row, col);
            let projections: Option<Vec<f64>> = slices
                .iter()
                .map(|s| s.position.map(|p| dot(p, normal)))
                .collect();
            if let Some(proj) = projections {
                let mut sorted = proj.clone();
                sorted.sort_by(|a, b| a.total_cmp(b));
                let min_gap = sorted.windows(2).map(|w| w[1] - w[0]).fold(f64::MAX, f64::min);
                if min_gap > 1e-3 {
                    let mut order: Vec<usize> = (0..slices.len()).collect();
                    order.sort_by(|&a, &b| proj[a].total_cmp(&proj[b]));
                    apply_order(slices, &order);
                    return "geometric";
                }
                warn!(
                    min_gap,
                    "duplicate/near-duplicate slice positions; falling back to InstanceNumber"
                );
            }
        }
    }
    slices.sort_by_key(|s| s.instance_number);
    "instance-number"
}

fn apply_order(slices: &mut [SliceRef], order: &[usize]) {
    let reordered: Vec<SliceRef> = order.iter().map(|&i| slices[i].clone()).collect();
    slices.clone_from_slice(&reordered);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(id: &str, num: i64, z: f64) -> SliceRef {
        SliceRef { id: id.into(), instance_number: num, position: Some([0.0, 0.0, z]), frames: 1 }
    }

    const AXIAL: Option<[[f64; 3]; 2]> = Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);

    #[test]
    fn geometry_beats_instance_number() {
        // Instance numbers reversed relative to spatial order — the bug case.
        let mut s = vec![slice("a", 3, 0.0), slice("b", 2, 2.5), slice("c", 1, 5.0)];
        let method = sort_slices(AXIAL, &mut s);
        assert_eq!(method, "geometric");
        assert_eq!(
            s.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "spatial order, not instance-number order"
        );
    }

    #[test]
    fn oblique_series_projects_onto_normal() {
        // Coronal-ish orientation: normal points along -Y; sort must follow
        // the projection, not raw z.
        let orient = Some([[1.0, 0.0, 0.0], [0.0, 0.0, -1.0]]);
        let mut s = vec![
            SliceRef { id: "far".into(), instance_number: 1, position: Some([0.0, 30.0, 0.0]), frames: 1 },
            SliceRef { id: "near".into(), instance_number: 2, position: Some([0.0, 10.0, 0.0]), frames: 1 },
        ];
        assert_eq!(sort_slices(orient, &mut s), "geometric");
        assert_eq!(s[0].id, "near", "normal is (0,1,0): smaller y projects smaller");
    }

    #[test]
    fn missing_position_falls_back() {
        let mut s = vec![slice("a", 2, 0.0), SliceRef {
            id: "b".into(), instance_number: 1, position: None, frames: 1,
        }];
        assert_eq!(sort_slices(AXIAL, &mut s), "instance-number");
        assert_eq!(s[0].id, "b");
    }

    #[test]
    fn missing_orientation_falls_back() {
        let mut s = vec![slice("a", 2, 5.0), slice("b", 1, 0.0)];
        assert_eq!(sort_slices(None, &mut s), "instance-number");
        assert_eq!(s[0].id, "b");
    }

    #[test]
    fn duplicate_positions_fall_back() {
        // Multi-phase acquisition: same table position twice.
        let mut s = vec![slice("a", 2, 0.0), slice("b", 1, 0.0), slice("c", 3, 2.5)];
        assert_eq!(sort_slices(AXIAL, &mut s), "instance-number");
        assert_eq!(s[0].id, "b");
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_position("1\\2\\3.5"), Some([1.0, 2.0, 3.5]));
        assert_eq!(parse_position("1\\2"), None);
        assert_eq!(
            parse_orientation("1\\0\\0\\0\\1\\0"),
            Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]])
        );
        assert_eq!(parse_orientation("bogus"), None);
    }
}
