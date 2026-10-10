//! The weak rolling checksum zsync uses to find candidate block matches.
//!
//! This is the rsync rolling checksum as zsync 0.6.2 computes it: two
//! wrapping 16-bit halves, `a` the plain sum of the window's bytes and `b`
//! each byte weighted by its distance from the end of the window. Sliding
//! the window forward one byte is two additions, not a fresh pass.

/// A rolling checksum over one window of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rsum {
    pub a: u16,
    pub b: u16,
}

impl Rsum {
    /// Checksum of `window`, computed from scratch.
    pub fn of(window: &[u8]) -> Self {
        let mut a: u16 = 0;
        let mut b: u16 = 0;
        // Weight counts down: first byte × len, last byte × 1.
        let mut weight = window.len() as u16;
        for &byte in window {
            let byte = u16::from(byte);
            a = a.wrapping_add(byte);
            b = b.wrapping_add(weight.wrapping_mul(byte));
            weight = weight.wrapping_sub(1);
        }
        Self { a, b }
    }

    /// Slide the window forward one byte. `blockshift` is log2(window length);
    /// the block size is always a power of two, so `leaving << blockshift`
    /// is `leaving × blocksize`.
    pub fn roll(&mut self, leaving: u8, entering: u8, blockshift: u32) {
        let leaving = u16::from(leaving);
        let entering = u16::from(entering);
        self.a = self.a.wrapping_add(entering).wrapping_sub(leaving);
        // b is updated with the NEW a.
        let out = if blockshift < 16 {
            leaving << blockshift
        } else {
            0
        };
        self.b = self.b.wrapping_add(self.a).wrapping_sub(out);
    }

    /// Both halves as one value, `a` in the high half.
    pub fn value(self) -> u32 {
        (u32::from(self.a) << 16) | u32::from(self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roll_matches_fresh_computation() {
        let data: Vec<u8> = (0..512u32).map(|i| ((i * 37 + 11) % 251) as u8).collect();
        for blocksize in [16usize, 64, 256] {
            let shift = blocksize.trailing_zeros();
            let mut rolled = Rsum::of(&data[..blocksize]);
            for start in 0..(data.len() - blocksize) {
                rolled.roll(data[start], data[start + blocksize], shift);
                assert_eq!(rolled, Rsum::of(&data[start + 1..start + 1 + blocksize]));
            }
        }
    }

    #[test]
    fn wrapping_behavior() {
        // 0xff bytes overflow both halves; rolling must agree with fresh.
        let data = vec![0xffu8; 300];
        let mut rolled = Rsum::of(&data[..128]);
        for start in 0..(data.len() - 128) {
            rolled.roll(data[start], data[start + 128], 7);
            assert_eq!(rolled, Rsum::of(&data[start + 1..start + 129]));
        }
    }

    #[test]
    fn known_values() {
        // Hand-computed: window [1,2,3,4]: a=10, b=1*4+2*3+3*2+4*1=20.
        let r = Rsum::of(&[1, 2, 3, 4]);
        assert_eq!((r.a, r.b), (10, 20));
        // Rolling [1,2,3,4] -> [2,3,4,5]: a=14, b=2*4+3*3+4*2+5=30.
        let mut r = r;
        r.roll(1, 5, 2);
        assert_eq!((r.a, r.b), (14, 30));
        assert_eq!(Rsum::of(&[2, 3, 4, 5]), r);
    }
}
