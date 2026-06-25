use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(raw: u64) -> Self {
                Self(raw)
            }

            pub const fn raw(self) -> u64 {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(EntityId);
id_type!(ClientId);
id_type!(ComponentId);
id_type!(RegionId);
id_type!(StreamId);
id_type!(SnapshotId);

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tick(u64);

impl Tick {
    pub const ZERO: Self = Self(0);

    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub fn advance(&mut self) -> Self {
        self.0 = self.0.saturating_add(1);
        *self
    }
}

impl fmt::Debug for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tick({})", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn distance_squared(self, other: Self) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        let dz = self.z - other.z;
        dx.mul_add(dx, dy.mul_add(dy, dz * dz))
    }

    pub fn lerp(self, other: Self, amount: f32) -> Self {
        Self {
            x: self.x + (other.x - self.x) * amount,
            y: self.y + (other.y - self.y) * amount,
            z: self.z + (other.z - self.z) * amount,
        }
    }

    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl Default for Vec3 {
    fn default() -> Self {
        Self::ZERO
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteBudget {
    remaining: usize,
}

impl ByteBudget {
    pub const fn new(bytes: usize) -> Self {
        Self { remaining: bytes }
    }

    pub const fn remaining(self) -> usize {
        self.remaining
    }

    pub const fn is_empty(self) -> bool {
        self.remaining == 0
    }

    pub fn try_reserve(&mut self, bytes: usize) -> bool {
        if bytes <= self.remaining {
            self.remaining -= bytes;
            true
        } else {
            false
        }
    }
}

pub trait SpatialPosition {
    fn position(&self) -> Vec3;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_budget_reserves_until_empty() {
        let mut budget = ByteBudget::new(12);

        assert!(budget.try_reserve(5));
        assert_eq!(budget.remaining(), 7);
        assert!(!budget.try_reserve(8));
        assert!(budget.try_reserve(7));
        assert!(budget.is_empty());
    }

    #[test]
    fn ticks_advance_saturating() {
        let mut tick = Tick::new(u64::MAX - 1);

        assert_eq!(tick.advance().raw(), u64::MAX);
        assert_eq!(tick.advance().raw(), u64::MAX);
    }

    #[test]
    fn vec3_lerp_interpolates_components() {
        assert_eq!(
            Vec3::new(1.0, 2.0, 3.0).lerp(Vec3::new(5.0, 10.0, 15.0), 0.25),
            Vec3::new(2.0, 4.0, 6.0)
        );
    }
}
