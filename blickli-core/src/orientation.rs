//! EXIF orientations (1–8) and composition with view rotations and flips.

/// Horizontal flip followed by clockwise 90° turns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Orientation {
    pub mirrored: bool,
    /// Clockwise 90° turns (0–3).
    pub turns: u8,
}

impl Orientation {
    pub const UP: Self = Self {
        mirrored: false,
        turns: 0,
    };

    pub fn new(mirrored: bool, turns: i32) -> Self {
        Self {
            mirrored,
            turns: turns.rem_euclid(4) as u8,
        }
    }

    pub fn from_view(degrees: i32, flipped: bool) -> Self {
        Self::new(flipped, degrees.div_euclid(90))
    }

    pub fn degrees(self) -> i32 {
        i32::from(self.turns) * 90
    }

    /// Convert an EXIF/TIFF orientation tag (1–8); invalid values default to upright.
    pub fn from_exif(tag: i64) -> Self {
        match tag {
            2 => Self::new(true, 0),
            3 => Self::new(false, 2),
            4 => Self::new(true, 2),
            5 => Self::new(true, 3),
            6 => Self::new(false, 1),
            7 => Self::new(true, 1),
            8 => Self::new(false, 3),
            _ => Self::UP,
        }
    }

    pub fn exif(self) -> u8 {
        match (self.mirrored, self.turns) {
            (false, 0) => 1,
            (true, 0) => 2,
            (false, 2) => 3,
            (true, 2) => 4,
            (true, 3) => 5,
            (false, 1) => 6,
            (true, 1) => 7,
            _ => 8,
        }
    }

    /// Compose `self` followed by `next`.
    pub fn then(self, next: Self) -> Self {
        // A mirror reverses the direction of prior turns.
        let turns = if next.mirrored {
            i32::from(next.turns) - i32::from(self.turns)
        } else {
            i32::from(next.turns) + i32::from(self.turns)
        };
        Self::new(self.mirrored != next.mirrored, turns)
    }

    pub fn inverse(self) -> Self {
        if self.mirrored {
            // Reflections are self-inverse.
            self
        } else {
            Self::new(false, -i32::from(self.turns))
        }
    }

    pub fn swaps_sides(self) -> bool {
        self.turns % 2 == 1
    }

    /// Transform a centered point in screen coordinates (y-down).
    #[cfg(test)]
    pub fn apply(self, (x, y): (f64, f64)) -> (f64, f64) {
        let (mut x, mut y) = if self.mirrored { (-x, y) } else { (x, y) };
        for _ in 0..self.turns {
            (x, y) = (-y, x);
        }
        (x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> impl Iterator<Item = Orientation> {
        (0..8).map(|n| Orientation::new(n >= 4, n % 4))
    }

    fn same(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn the_tags_mean_what_the_exif_standard_says() {
        // Probe point in upper-right quadrant.
        let p = (2.0, -1.0);
        let cases: [(u8, (f64, f64)); 8] = [
            (1, (2.0, -1.0)),  // upright
            (2, (-2.0, -1.0)), // flip horizontal
            (3, (-2.0, 1.0)),  // rotate 180
            (4, (2.0, 1.0)),   // flip vertical
            (5, (-1.0, 2.0)),  // transpose
            (6, (1.0, 2.0)),   // rotate 90 CW
            (7, (1.0, -2.0)),  // transverse
            (8, (-1.0, -2.0)), // rotate 90 CCW
        ];
        for (tag, expected) in cases {
            let o = Orientation::from_exif(i64::from(tag));
            assert!(same(o.apply(p), expected), "tag {tag}: {:?}", o.apply(p));
            assert_eq!(o.exif(), tag);
        }
        assert_eq!(Orientation::from_exif(0), Orientation::UP);
        assert_eq!(Orientation::from_exif(9), Orientation::UP);
    }

    #[test]
    fn composing_matches_applying_one_after_the_other() {
        let p = (3.0, -2.0);
        for a in all() {
            for b in all() {
                assert!(
                    same(a.then(b).apply(p), b.apply(a.apply(p))),
                    "{a:?} then {b:?}"
                );
            }
        }
    }

    #[test]
    fn every_orientation_can_be_undone() {
        for o in all() {
            assert_eq!(o.then(o.inverse()), Orientation::UP, "{o:?}");
            assert_eq!(o.inverse().then(o), Orientation::UP, "{o:?}");
        }
    }

    #[test]
    fn a_turned_photo_turned_again_saves_the_right_tag() {
        // EXIF 6 (90° CW) + 90° CW = EXIF 3 (180°).
        let file = Orientation::from_exif(6);
        let view = Orientation::from_view(90, false);
        assert_eq!(file.then(view).exif(), 3);
        let back = Orientation::from_view(-90, false);
        assert_eq!(file.then(back).exif(), 1);
        // EXIF 6 + horizontal flip = EXIF 5 (transpose).
        let flipped = Orientation::from_view(0, true);
        assert_eq!(file.then(flipped).exif(), 5);
        assert_eq!(Orientation::from_view(270, false).degrees(), 270);
        assert!(view.swaps_sides() && !file.then(view).swaps_sides());
    }
}
