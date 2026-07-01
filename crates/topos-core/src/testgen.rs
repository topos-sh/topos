//! Test-only deterministic input generation — a tiny xorshift64* shared by the kernel's generative
//! tests (the same house pattern the gitstore fuzz uses). Seeded and reproducible, so a failing case
//! is a fixed regression, and dependency-free: the kernel itself stays RNG-free even under test.

pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}
