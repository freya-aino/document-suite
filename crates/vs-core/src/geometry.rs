use serde::{Deserialize, Serialize};

use crate::CoreError;

/// Bounding box normalised to `[0, 1]` relative to the page, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl BBox {
    pub fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Result<Self, CoreError> {
        let in_range = |v: f32| (0.0..=1.0).contains(&v);
        if [x0, y0, x1, y1].iter().all(|v| in_range(*v)) && x0 <= x1 && y0 <= y1 {
            Ok(BBox { x0, y0, x1, y1 })
        } else {
            Err(CoreError::InvalidBBox(format!("({x0},{y0},{x1},{y1})")))
        }
    }

    /// Build from pixel coordinates of a page rendered at `width` x `height`.
    pub fn from_pixels(x0: f32, y0: f32, x1: f32, y1: f32, width: f32, height: f32) -> Result<Self, CoreError> {
        let clamp = |v: f32| v.clamp(0.0, 1.0);
        BBox::new(clamp(x0 / width), clamp(y0 / height), clamp(x1 / width), clamp(y1 / height))
    }

    pub fn area(&self) -> f32 {
        (self.x1 - self.x0) * (self.y1 - self.y0)
    }

    pub fn intersection_over_union(&self, other: &BBox) -> f32 {
        let ix0 = self.x0.max(other.x0);
        let iy0 = self.y0.max(other.y0);
        let ix1 = self.x1.min(other.x1);
        let iy1 = self.y1.min(other.y1);
        let inter = (ix1 - ix0).max(0.0) * (iy1 - iy0).max(0.0);
        let union = self.area() + other.area() - inter;
        if union <= 0.0 { 0.0 } else { inter / union }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_out_of_range() {
        assert!(BBox::new(0.0, 0.0, 1.1, 0.5).is_err());
        assert!(BBox::new(0.5, 0.0, 0.4, 0.5).is_err());
    }

    #[test]
    fn iou() {
        let a = BBox::new(0.0, 0.0, 0.5, 0.5).unwrap();
        assert!((a.intersection_over_union(&a) - 1.0).abs() < 1e-6);
        let b = BBox::new(0.5, 0.5, 1.0, 1.0).unwrap();
        assert_eq!(a.intersection_over_union(&b), 0.0);
    }
}
