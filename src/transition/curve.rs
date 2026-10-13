//! Automation curves over a transition's normalised time `t` in `0..=1`.
//!
//! A curve is a list of pieces, each covering `[start, end]` of the overlap.
//! Inside a piece, `x` runs 0..1. Four points at x = 0, 1/3, 2/3, 1 are a
//! cubic Bézier in y (x is linear in the parameter, so no x solve is needed);
//! any other point list is piecewise linear, where two points at the same x
//! make a step. Values are in the unit of the parameter they drive: linear
//! gain, dB, Hz or Q. Outside every piece the nearest piece's edge value holds.

use protobuf::CodedInputStream;

#[derive(Clone, Debug, PartialEq)]
pub struct Piece {
    pub start: f32,
    pub end: f32,
    pub points: Vec<(f32, f32)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Curve {
    pub pieces: Vec<Piece>,
}

impl Curve {
    /// A curve that holds `value` throughout.
    pub fn constant(value: f32) -> Self {
        Self::linear(&[(0.0, value), (1.0, value)])
    }

    /// One piece over the whole overlap through `points`.
    pub fn linear(points: &[(f32, f32)]) -> Self {
        Self::piece(0.0, 1.0, points)
    }

    pub fn piece(start: f32, end: f32, points: &[(f32, f32)]) -> Self {
        Self {
            pieces: vec![Piece {
                start,
                end,
                points: points.to_vec(),
            }],
        }
    }

    /// Appends another piece; pieces must be added in time order.
    pub fn then(mut self, start: f32, end: f32, points: &[(f32, f32)]) -> Self {
        self.pieces.push(Piece {
            start,
            end,
            points: points.to_vec(),
        });
        self
    }

    pub fn eval(&self, t: f32) -> f32 {
        let Some(first) = self.pieces.first() else {
            return 0.0;
        };
        if t <= first.start {
            return first.at(0.0);
        }
        let mut held = first.at(0.0);
        for piece in &self.pieces {
            if t < piece.start {
                return held;
            }
            if t <= piece.end {
                let span = piece.end - piece.start;
                let x = if span > 0.0 {
                    (t - piece.start) / span
                } else {
                    1.0
                };
                return piece.at(x);
            }
            held = piece.at(1.0);
        }
        held
    }

    /// AutomationCurves { repeated AutomationCurve curves = 1 }, each
    /// { repeated Point points = 1; double start = 2; double end = 3 } and
    /// Point { double x = 1; double y = 2 }.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut curve = Self::default();
        let mut input = CodedInputStream::from_bytes(bytes);
        while let Some(tag) = input.read_raw_tag_or_eof()? {
            match tag {
                10 => curve.pieces.push(decode_piece(&input.read_bytes()?)?),
                _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
            }
        }
        Ok(curve)
    }
}

fn decode_piece(bytes: &[u8]) -> anyhow::Result<Piece> {
    let mut piece = Piece {
        start: 0.0,
        end: 0.0,
        points: Vec::new(),
    };
    let mut input = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        match tag {
            10 => {
                let point = input.read_bytes()?;
                let mut input = CodedInputStream::from_bytes(&point);
                let (mut x, mut y) = (0.0, 0.0);
                while let Some(tag) = input.read_raw_tag_or_eof()? {
                    match tag {
                        9 => x = input.read_double()? as f32,
                        17 => y = input.read_double()? as f32,
                        _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
                    }
                }
                piece.points.push((x, y));
            }
            17 => piece.start = input.read_double()? as f32,
            25 => piece.end = input.read_double()? as f32,
            _ => protobuf::rt::skip_field_for_tag(tag, &mut input)?,
        }
    }
    Ok(piece)
}

impl Piece {
    fn is_bezier(&self) -> bool {
        const THIRDS: [f32; 4] = [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0];
        self.points.len() == 4
            && self
                .points
                .iter()
                .zip(THIRDS)
                .all(|((x, _), third)| (x - third).abs() < 1e-3)
    }

    fn at(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self.points.as_slice() {
            [] => 0.0,
            [(_, y)] => *y,
            [(_, y0), (_, c1), (_, c2), (_, y1)] if self.is_bezier() => {
                let u = 1.0 - x;
                u * u * u * y0 + 3.0 * u * u * x * c1 + 3.0 * u * x * x * c2 + x * x * x * y1
            }
            points => {
                // The last point at or before x and the first after it; a
                // step (two points at one x) takes the later value from x on.
                let mut previous = points[0];
                if x <= previous.0 {
                    return previous.1;
                }
                for &point in &points[1..] {
                    if x < point.0 {
                        let span = point.0 - previous.0;
                        let along = if span > 0.0 {
                            (x - previous.0) / span
                        } else {
                            1.0
                        };
                        return previous.1 + (point.1 - previous.1) * along;
                    }
                    previous = point;
                }
                previous.1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn linear_piece_interpolates_and_holds_outside() {
        let curve = Curve::piece(0.25, 0.75, &[(0.0, 1.0), (1.0, 0.0)]);
        assert!(close(curve.eval(0.0), 1.0));
        assert!(close(curve.eval(0.5), 0.5));
        assert!(close(curve.eval(1.0), 0.0));
    }

    #[test]
    fn a_step_switches_at_its_x() {
        let curve = Curve::linear(&[(0.0, 1.0), (0.6, 1.0), (0.6, 0.0), (1.0, 0.0)]);
        assert!(close(curve.eval(0.59), 1.0));
        assert!(close(curve.eval(0.6), 0.0));
        assert!(close(curve.eval(0.9), 0.0));
    }

    #[test]
    fn four_points_at_thirds_are_a_bezier() {
        let curve = Curve::linear(&[(0.0, 0.0), (1.0 / 3.0, 1.0), (2.0 / 3.0, 1.0), (1.0, 0.0)]);
        // y(0.5) = 3/8 + 3/8 = 0.75, which a polyline through the points would give as 1.
        assert!(close(curve.eval(0.5), 0.75));
        assert!(close(curve.eval(0.0), 0.0));
        assert!(close(curve.eval(1.0), 0.0));
    }

    #[test]
    fn gaps_between_pieces_hold_the_previous_end() {
        let curve = Curve::piece(0.0, 0.2, &[(0.0, 0.0), (1.0, 1.0)]).then(
            0.8,
            1.0,
            &[(0.0, 1.0), (1.0, 3.0)],
        );
        assert!(close(curve.eval(0.5), 1.0));
        assert!(close(curve.eval(0.9), 2.0));
        assert!(close(curve.eval(2.0), 3.0));
    }

    #[test]
    fn decodes_automation_curves() {
        // One piece 0.5..1 with points (0,1) and (1,0).
        let mut point_a = vec![0x09];
        point_a.extend(0.0f64.to_le_bytes());
        point_a.push(0x11);
        point_a.extend(1.0f64.to_le_bytes());
        let mut point_b = vec![0x09];
        point_b.extend(1.0f64.to_le_bytes());
        point_b.push(0x11);
        point_b.extend(0.0f64.to_le_bytes());
        let mut piece = vec![0x0a, point_a.len() as u8];
        piece.extend(&point_a);
        piece.extend([0x0a, point_b.len() as u8]);
        piece.extend(&point_b);
        piece.push(0x11);
        piece.extend(0.5f64.to_le_bytes());
        piece.push(0x19);
        piece.extend(1.0f64.to_le_bytes());
        let mut bytes = vec![0x0a, piece.len() as u8];
        bytes.extend(&piece);
        let curve = Curve::decode(&bytes).unwrap();
        assert_eq!(curve.pieces.len(), 1);
        assert!(close(curve.eval(0.25), 1.0));
        assert!(close(curve.eval(0.75), 0.5));
    }
}
