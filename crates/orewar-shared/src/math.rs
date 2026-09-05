//! Small 2D math kit.
//!
//! The shared crate deliberately has zero dependencies so that the server can be
//! built and run without dragging in a renderer, so we carry our own `Vec2`
//! rather than borrowing Bevy's. The client converts at the boundary.

use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

pub const TAU: f32 = std::f32::consts::TAU;
pub const PI: f32 = std::f32::consts::PI;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

pub const fn vec2(x: f32, y: f32) -> Vec2 {
    Vec2 { x, y }
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    pub const fn splat(v: f32) -> Self {
        Self { x: v, y: v }
    }

    /// Unit vector pointing along `radians`, measured counter-clockwise from +X.
    pub fn from_angle(radians: f32) -> Self {
        Self { x: radians.cos(), y: radians.sin() }
    }

    pub fn to_angle(self) -> f32 {
        self.y.atan2(self.x)
    }

    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y
    }

    /// 2D cross product (the z component of the 3D cross). Sign tells you which
    /// side `o` lies on, which is how the missile picks a turn direction.
    pub fn cross(self, o: Self) -> f32 {
        self.x * o.y - self.y * o.x
    }

    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    pub fn distance_squared(self, o: Self) -> f32 {
        (self - o).length_squared()
    }

    pub fn distance(self, o: Self) -> f32 {
        (self - o).length()
    }

    pub fn normalize_or_zero(self) -> Self {
        let len = self.length();
        if len > 1e-6 { self / len } else { Self::ZERO }
    }

    pub fn perp(self) -> Self {
        Self { x: -self.y, y: self.x }
    }

    pub fn lerp(self, o: Self, t: f32) -> Self {
        self + (o - self) * t
    }

    pub fn clamp_length_max(self, max: f32) -> Self {
        let len = self.length();
        if len > max && len > 1e-6 { self * (max / len) } else { self }
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl Add for Vec2 {
    type Output = Vec2;
    fn add(self, o: Vec2) -> Vec2 {
        vec2(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, o: Vec2) -> Vec2 {
        vec2(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f32> for Vec2 {
    type Output = Vec2;
    fn mul(self, s: f32) -> Vec2 {
        vec2(self.x * s, self.y * s)
    }
}

impl Div<f32> for Vec2 {
    type Output = Vec2;
    fn div(self, s: f32) -> Vec2 {
        vec2(self.x / s, self.y / s)
    }
}

impl Neg for Vec2 {
    type Output = Vec2;
    fn neg(self) -> Vec2 {
        vec2(-self.x, -self.y)
    }
}

impl AddAssign for Vec2 {
    fn add_assign(&mut self, o: Vec2) {
        *self = *self + o;
    }
}

impl SubAssign for Vec2 {
    fn sub_assign(&mut self, o: Vec2) {
        *self = *self - o;
    }
}

/// Wraps an angle into `(-PI, PI]`.
pub fn wrap_angle(a: f32) -> f32 {
    let mut a = (a + PI) % TAU;
    if a < 0.0 {
        a += TAU;
    }
    a - PI
}

/// Signed shortest rotation to get from `from` to `to`.
pub fn angle_delta(from: f32, to: f32) -> f32 {
    wrap_angle(to - from)
}

/// Interpolate angles the short way around the circle.
pub fn angle_lerp(from: f32, to: f32, t: f32) -> f32 {
    wrap_angle(from + angle_delta(from, to) * t)
}

/// Move `from` toward `to` by at most `max_step`.
pub fn angle_approach(from: f32, to: f32, max_step: f32) -> f32 {
    let d = angle_delta(from, to);
    if d.abs() <= max_step { wrap_angle(to) } else { wrap_angle(from + d.signum() * max_step) }
}

/// Move a scalar toward a target by at most `max_step`.
pub fn approach(from: f32, to: f32, max_step: f32) -> f32 {
    let d = to - from;
    if d.abs() <= max_step { to } else { from + d.signum() * max_step }
}

/// Frame-rate independent exponential smoothing.
///
/// `half_life` is the time for the remaining error to halve, so the result does
/// not change character when the frame time does.
pub fn smooth_damp(from: f32, to: f32, half_life: f32, dt: f32) -> f32 {
    if half_life <= 0.0 {
        return to;
    }
    let t = 1.0 - (-dt * std::f32::consts::LN_2 / half_life).exp();
    from + (to - from) * t
}

pub fn smooth_damp_vec2(from: Vec2, to: Vec2, half_life: f32, dt: f32) -> Vec2 {
    if half_life <= 0.0 {
        return to;
    }
    let t = 1.0 - (-dt * std::f32::consts::LN_2 / half_life).exp();
    from.lerp(to, t)
}
