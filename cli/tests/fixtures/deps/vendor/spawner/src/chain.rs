// A chain of names, each bound by `use` items behind `cfg`s that can't be
// known to any of the names a step before it. A path through a name at the
// end may mean any of the names at the start, through any of the 5 to the
// power of the chain's length ways to them. The call counts for each item it
// may mean, without following each way there.
#[cfg(spawner_a)] use std::mem::{drop as a0, drop as b0, drop as c0, drop as d0, drop as e0};
#[cfg(spawner_b)] use std::mem::{drop as a0, drop as b0, drop as c0, drop as d0, drop as e0};
#[cfg(spawner_c)] use std::mem::{drop as a0, drop as b0, drop as c0, drop as d0, drop as e0};
#[cfg(spawner_d)] use std::mem::{drop as a0, drop as b0, drop as c0, drop as d0, drop as e0};
#[cfg(spawner_e)] use std::mem::{drop as a0, drop as b0, drop as c0, drop as d0, forget as e0};
#[cfg(spawner_a)] use {a0 as a1, b0 as b1, c0 as c1, d0 as d1, e0 as e1};
#[cfg(spawner_b)] use {b0 as a1, c0 as b1, d0 as c1, e0 as d1, a0 as e1};
#[cfg(spawner_c)] use {c0 as a1, d0 as b1, e0 as c1, a0 as d1, b0 as e1};
#[cfg(spawner_d)] use {d0 as a1, e0 as b1, a0 as c1, b0 as d1, c0 as e1};
#[cfg(spawner_e)] use {e0 as a1, a0 as b1, b0 as c1, c0 as d1, d0 as e1};
#[cfg(spawner_a)] use {a1 as a2, b1 as b2, c1 as c2, d1 as d2, e1 as e2};
#[cfg(spawner_b)] use {b1 as a2, c1 as b2, d1 as c2, e1 as d2, a1 as e2};
#[cfg(spawner_c)] use {c1 as a2, d1 as b2, e1 as c2, a1 as d2, b1 as e2};
#[cfg(spawner_d)] use {d1 as a2, e1 as b2, a1 as c2, b1 as d2, c1 as e2};
#[cfg(spawner_e)] use {e1 as a2, a1 as b2, b1 as c2, c1 as d2, d1 as e2};
#[cfg(spawner_a)] use {a2 as a3, b2 as b3, c2 as c3, d2 as d3, e2 as e3};
#[cfg(spawner_b)] use {b2 as a3, c2 as b3, d2 as c3, e2 as d3, a2 as e3};
#[cfg(spawner_c)] use {c2 as a3, d2 as b3, e2 as c3, a2 as d3, b2 as e3};
#[cfg(spawner_d)] use {d2 as a3, e2 as b3, a2 as c3, b2 as d3, c2 as e3};
#[cfg(spawner_e)] use {e2 as a3, a2 as b3, b2 as c3, c2 as d3, d2 as e3};
#[cfg(spawner_a)] use {a3 as a4, b3 as b4, c3 as c4, d3 as d4, e3 as e4};
#[cfg(spawner_b)] use {b3 as a4, c3 as b4, d3 as c4, e3 as d4, a3 as e4};
#[cfg(spawner_c)] use {c3 as a4, d3 as b4, e3 as c4, a3 as d4, b3 as e4};
#[cfg(spawner_d)] use {d3 as a4, e3 as b4, a3 as c4, b3 as d4, c3 as e4};
#[cfg(spawner_e)] use {e3 as a4, a3 as b4, b3 as c4, c3 as d4, d3 as e4};
#[cfg(spawner_a)] use {a4 as a5, b4 as b5, c4 as c5, d4 as d5, e4 as e5};
#[cfg(spawner_b)] use {b4 as a5, c4 as b5, d4 as c5, e4 as d5, a4 as e5};
#[cfg(spawner_c)] use {c4 as a5, d4 as b5, e4 as c5, a4 as d5, b4 as e5};
#[cfg(spawner_d)] use {d4 as a5, e4 as b5, a4 as c5, b4 as d5, c4 as e5};
#[cfg(spawner_e)] use {e4 as a5, a4 as b5, b4 as c5, c4 as d5, d4 as e5};
#[cfg(spawner_a)] use {a5 as a6, b5 as b6, c5 as c6, d5 as d6, e5 as e6};
#[cfg(spawner_b)] use {b5 as a6, c5 as b6, d5 as c6, e5 as d6, a5 as e6};
#[cfg(spawner_c)] use {c5 as a6, d5 as b6, e5 as c6, a5 as d6, b5 as e6};
#[cfg(spawner_d)] use {d5 as a6, e5 as b6, a5 as c6, b5 as d6, c5 as e6};
#[cfg(spawner_e)] use {e5 as a6, a5 as b6, b5 as c6, c5 as d6, d5 as e6};
#[cfg(spawner_a)] use {a6 as a7, b6 as b7, c6 as c7, d6 as d7, e6 as e7};
#[cfg(spawner_b)] use {b6 as a7, c6 as b7, d6 as c7, e6 as d7, a6 as e7};
#[cfg(spawner_c)] use {c6 as a7, d6 as b7, e6 as c7, a6 as d7, b6 as e7};
#[cfg(spawner_d)] use {d6 as a7, e6 as b7, a6 as c7, b6 as d7, c6 as e7};
#[cfg(spawner_e)] use {e6 as a7, a6 as b7, b6 as c7, c6 as d7, d6 as e7};
#[cfg(spawner_a)] use {a7 as a8, b7 as b8, c7 as c8, d7 as d8, e7 as e8};
#[cfg(spawner_b)] use {b7 as a8, c7 as b8, d7 as c8, e7 as d8, a7 as e8};
#[cfg(spawner_c)] use {c7 as a8, d7 as b8, e7 as c8, a7 as d8, b7 as e8};
#[cfg(spawner_d)] use {d7 as a8, e7 as b8, a7 as c8, b7 as d8, c7 as e8};
#[cfg(spawner_e)] use {e7 as a8, a7 as b8, b7 as c8, c7 as d8, d7 as e8};
#[cfg(spawner_a)] use {a8 as a9, b8 as b9, c8 as c9, d8 as d9, e8 as e9};
#[cfg(spawner_b)] use {b8 as a9, c8 as b9, d8 as c9, e8 as d9, a8 as e9};
#[cfg(spawner_c)] use {c8 as a9, d8 as b9, e8 as c9, a8 as d9, b8 as e9};
#[cfg(spawner_d)] use {d8 as a9, e8 as b9, a8 as c9, b8 as d9, c8 as e9};
#[cfg(spawner_e)] use {e8 as a9, a8 as b9, b8 as c9, c8 as d9, d8 as e9};
#[cfg(spawner_a)] use {a9 as a10, b9 as b10, c9 as c10, d9 as d10, e9 as e10};
#[cfg(spawner_b)] use {b9 as a10, c9 as b10, d9 as c10, e9 as d10, a9 as e10};
#[cfg(spawner_c)] use {c9 as a10, d9 as b10, e9 as c10, a9 as d10, b9 as e10};
#[cfg(spawner_d)] use {d9 as a10, e9 as b10, a9 as c10, b9 as d10, c9 as e10};
#[cfg(spawner_e)] use {e9 as a10, a9 as b10, b9 as c10, c9 as d10, d9 as e10};
#[cfg(spawner_a)] use {a10 as a11, b10 as b11, c10 as c11, d10 as d11, e10 as e11};
#[cfg(spawner_b)] use {b10 as a11, c10 as b11, d10 as c11, e10 as d11, a10 as e11};
#[cfg(spawner_c)] use {c10 as a11, d10 as b11, e10 as c11, a10 as d11, b10 as e11};
#[cfg(spawner_d)] use {d10 as a11, e10 as b11, a10 as c11, b10 as d11, c10 as e11};
#[cfg(spawner_e)] use {e10 as a11, a10 as b11, b10 as c11, c10 as d11, d10 as e11};
#[cfg(spawner_a)] use {a11 as a12, b11 as b12, c11 as c12, d11 as d12, e11 as e12};
#[cfg(spawner_b)] use {b11 as a12, c11 as b12, d11 as c12, e11 as d12, a11 as e12};
#[cfg(spawner_c)] use {c11 as a12, d11 as b12, e11 as c12, a11 as d12, b11 as e12};
#[cfg(spawner_d)] use {d11 as a12, e11 as b12, a11 as c12, b11 as d12, c11 as e12};
#[cfg(spawner_e)] use {e11 as a12, a11 as b12, b11 as c12, c11 as d12, d11 as e12};
#[cfg(spawner_a)] use {a12 as a13, b12 as b13, c12 as c13, d12 as d13, e12 as e13};
#[cfg(spawner_b)] use {b12 as a13, c12 as b13, d12 as c13, e12 as d13, a12 as e13};
#[cfg(spawner_c)] use {c12 as a13, d12 as b13, e12 as c13, a12 as d13, b12 as e13};
#[cfg(spawner_d)] use {d12 as a13, e12 as b13, a12 as c13, b12 as d13, c12 as e13};
#[cfg(spawner_e)] use {e12 as a13, a12 as b13, b12 as c13, c12 as d13, d12 as e13};
#[cfg(spawner_a)] use {a13 as a14, b13 as b14, c13 as c14, d13 as d14, e13 as e14};
#[cfg(spawner_b)] use {b13 as a14, c13 as b14, d13 as c14, e13 as d14, a13 as e14};
#[cfg(spawner_c)] use {c13 as a14, d13 as b14, e13 as c14, a13 as d14, b13 as e14};
#[cfg(spawner_d)] use {d13 as a14, e13 as b14, a13 as c14, b13 as d14, c13 as e14};
#[cfg(spawner_e)] use {e13 as a14, a13 as b14, b13 as c14, c13 as d14, d13 as e14};

pub fn chained(value: Box<u8>) {
    a14(value); // std::mem::forget
}
