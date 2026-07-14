//! DBC signal value-variation engine.
//!
//! Each signal can carry a `VaryMode`. On every send (index `n`, starting at 0) the engine
//! recomputes the signal's physical value via `eval`. The result is the *physical* value;
//! the caller converts it to a raw value and fits it into the signal's bit width.
//!
//! Eight modes (matching the ZCANPRO "signal value variation" feature):
//!   None / Arithmetic cycle / Geometric cycle / Sine / Triangle / Rectangle / Random / Sequence
//!
//! Phase 1: pure engine + unit tests. Wiring into the send path / UI comes next.
#![allow(dead_code)]

/// How a signal's physical value changes with the send index `n`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum VaryMode {
    /// Fixed at the edited actual value.
    None,
    /// Cycle `lo, lo+|diff|, ... (<= hi)` then wrap back to `lo`.
    Arithmetic { diff: f64, lo: f64, hi: f64 },
    /// Cycle `init, init*ratio, ...` while inside `[lo, hi]`, then restart.
    Geometric {
        init: f64,
        ratio: f64,
        lo: f64,
        hi: f64,
    },
    /// `offset + amp*sin(omega*n + phase)`, saturated to `[lo, hi]`.
    Sine {
        amp: f64,
        omega: f64,
        phase: f64,
        offset: f64,
        lo: f64,
        hi: f64,
    },
    /// Triangle wave of `period` samples, `v_off + amp*tri`, saturated to `[lo, hi]`.
    Triangle {
        period: f64,
        amp: f64,
        h_off: f64,
        v_off: f64,
        lo: f64,
        hi: f64,
    },
    /// Rectangle wave of `period` samples, `high` while within `duty`, else `low`.
    Rect {
        period: f64,
        duty: f64,
        high: f64,
        low: f64,
    },
    /// Uniform random in `[lo, hi]` (the caller supplies the random fraction).
    Random { lo: f64, hi: f64 },
    /// Cycle through a user list of values.
    Sequence { values: Vec<f64> },
}

fn clampf(v: f64, lo: f64, hi: f64) -> f64 {
    let (lo, hi) = (lo.min(hi), lo.max(hi));
    v.max(lo).min(hi)
}

/// Compute the physical value at send index `n`.
/// `base` is the edited actual value (used by `None`/`Sequence` fallback);
/// `rand01` is a uniform fraction in `[0,1)` used only by `Random` (passed in for testability).
pub fn eval(mode: &VaryMode, n: u64, base: f64, rand01: f64) -> f64 {
    let nf = n as f64;
    match mode {
        VaryMode::None => base,

        VaryMode::Arithmetic { diff, lo, hi } => {
            let (lo, hi) = (lo.min(*hi), lo.max(*hi));
            let d = diff.abs();
            let span = hi - lo;
            if d == 0.0 || span <= 0.0 {
                return lo;
            }
            let steps = (span / d).floor() as u64 + 1; // lo .. <=hi inclusive
            let k = (n % steps) as f64;
            lo + k * d
        }

        VaryMode::Geometric {
            init,
            ratio,
            lo,
            hi,
        } => {
            let (lo, hi) = (lo.min(*hi), lo.max(*hi));
            if *ratio <= 0.0 || *init == 0.0 {
                return clampf(*init, lo, hi);
            }
            // number of steps that stay inside [lo, hi]
            let mut period = 1u64;
            let mut v = *init;
            if *ratio > 1.0 {
                while v * ratio <= hi && period < 1_000_000 {
                    v *= ratio;
                    period += 1;
                }
            } else if *ratio < 1.0 {
                while v * ratio >= lo && v * ratio > 0.0 && period < 1_000_000 {
                    v *= ratio;
                    period += 1;
                }
            }
            let k = (n % period) as i32;
            clampf(init * ratio.powi(k), lo, hi)
        }

        VaryMode::Sine {
            amp,
            omega,
            phase,
            offset,
            lo,
            hi,
        } => clampf(offset + amp * (omega * nf + phase).sin(), *lo, *hi),

        VaryMode::Triangle {
            period,
            amp,
            h_off,
            v_off,
            lo,
            hi,
        } => {
            let p = if *period <= 0.0 { 1.0 } else { *period };
            let x = ((nf + h_off).rem_euclid(p)) / p; // [0,1)
            let tri = 1.0 - 4.0 * (x - 0.5).abs(); // -1 at x=0/1, +1 at x=0.5
            clampf(v_off + amp * tri, *lo, *hi)
        }

        VaryMode::Rect {
            period,
            duty,
            high,
            low,
        } => {
            let p = if *period <= 0.0 { 1.0 } else { *period };
            let x = (nf.rem_euclid(p)) / p; // [0,1)
            if x < duty.clamp(0.0, 1.0) {
                *high
            } else {
                *low
            }
        }

        VaryMode::Random { lo, hi } => {
            let (lo, hi) = (lo.min(*hi), lo.max(*hi));
            lo + rand01.clamp(0.0, 1.0 - f64::EPSILON) * (hi - lo)
        }

        VaryMode::Sequence { values } => {
            if values.is_empty() {
                base
            } else {
                values[(n as usize) % values.len()]
            }
        }
    }
}

/// Truncate a raw signal value to `bits` bits (wrap-around), matching the "按有效位宽截取" rule.
pub fn truncate_to_bits(raw: i64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        return raw;
    }
    let mask = (1i64 << bits) - 1;
    raw & mask
}

/// Parse a comma/space/semicolon separated list of numbers for the custom-sequence mode.
pub fn parse_sequence(text: &str) -> Vec<f64> {
    text.split([',', ';', ' ', '\t', '\n'])
        .filter_map(|p| {
            let p = p.trim();
            if p.is_empty() {
                None
            } else {
                p.parse::<f64>().ok()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn none_is_fixed() {
        for n in 0..5 {
            approx(eval(&VaryMode::None, n, 42.5, 0.0), 42.5);
        }
    }

    #[test]
    fn arithmetic_cycles() {
        let m = VaryMode::Arithmetic {
            diff: 1.0,
            lo: 0.0,
            hi: 3.0,
        };
        let got: Vec<f64> = (0..6).map(|n| eval(&m, n, 0.0, 0.0)).collect();
        assert_eq!(got, vec![0.0, 1.0, 2.0, 3.0, 0.0, 1.0]);
    }

    #[test]
    fn arithmetic_step2() {
        let m = VaryMode::Arithmetic {
            diff: 2.0,
            lo: 0.0,
            hi: 5.0,
        };
        // steps = floor(5/2)+1 = 3 -> 0,2,4, wrap
        let got: Vec<f64> = (0..4).map(|n| eval(&m, n, 0.0, 0.0)).collect();
        assert_eq!(got, vec![0.0, 2.0, 4.0, 0.0]);
    }

    #[test]
    fn geometric_cycles() {
        let m = VaryMode::Geometric {
            init: 1.0,
            ratio: 2.0,
            lo: 1.0,
            hi: 8.0,
        };
        let got: Vec<f64> = (0..5).map(|n| eval(&m, n, 0.0, 0.0)).collect();
        assert_eq!(got, vec![1.0, 2.0, 4.0, 8.0, 1.0]);
    }

    #[test]
    fn sine_quarter_steps() {
        let m = VaryMode::Sine {
            amp: 1.0,
            omega: std::f64::consts::FRAC_PI_2,
            phase: 0.0,
            offset: 0.0,
            lo: -10.0,
            hi: 10.0,
        };
        approx(eval(&m, 0, 0.0, 0.0), 0.0);
        approx(eval(&m, 1, 0.0, 0.0), 1.0);
        approx(eval(&m, 2, 0.0, 0.0), 0.0);
        approx(eval(&m, 3, 0.0, 0.0), -1.0);
    }

    #[test]
    fn sine_saturates() {
        let m = VaryMode::Sine {
            amp: 100.0,
            omega: std::f64::consts::FRAC_PI_2,
            phase: 0.0,
            offset: 0.0,
            lo: -5.0,
            hi: 5.0,
        };
        approx(eval(&m, 1, 0.0, 0.0), 5.0); // 100 -> clamped to hi
        approx(eval(&m, 3, 0.0, 0.0), -5.0); // -100 -> clamped to lo
    }

    #[test]
    fn triangle_wave() {
        let m = VaryMode::Triangle {
            period: 4.0,
            amp: 1.0,
            h_off: 0.0,
            v_off: 0.0,
            lo: -10.0,
            hi: 10.0,
        };
        approx(eval(&m, 0, 0.0, 0.0), -1.0);
        approx(eval(&m, 1, 0.0, 0.0), 0.0);
        approx(eval(&m, 2, 0.0, 0.0), 1.0);
        approx(eval(&m, 3, 0.0, 0.0), 0.0);
    }

    #[test]
    fn rect_wave() {
        let m = VaryMode::Rect {
            period: 4.0,
            duty: 0.5,
            high: 1.0,
            low: 0.0,
        };
        let got: Vec<f64> = (0..5).map(|n| eval(&m, n, 0.0, 0.0)).collect();
        assert_eq!(got, vec![1.0, 1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn random_in_range() {
        let m = VaryMode::Random { lo: 10.0, hi: 20.0 };
        approx(eval(&m, 0, 0.0, 0.0), 10.0);
        approx(eval(&m, 0, 0.0, 0.5), 15.0);
        let v = eval(&m, 0, 0.0, 0.999);
        assert!((10.0..=20.0).contains(&v));
    }

    #[test]
    fn sequence_cycles() {
        let m = VaryMode::Sequence {
            values: vec![1.0, 2.5, -3.0],
        };
        let got: Vec<f64> = (0..4).map(|n| eval(&m, n, 0.0, 0.0)).collect();
        assert_eq!(got, vec![1.0, 2.5, -3.0, 1.0]);
    }

    #[test]
    fn sequence_empty_uses_base() {
        let m = VaryMode::Sequence { values: vec![] };
        approx(eval(&m, 3, 7.0, 0.0), 7.0);
    }

    #[test]
    fn truncate_bits() {
        assert_eq!(truncate_to_bits(0x1FF, 8), 0xFF);
        assert_eq!(truncate_to_bits(256, 8), 0);
        assert_eq!(truncate_to_bits(255, 8), 255);
        assert_eq!(truncate_to_bits(5, 0), 5);
    }

    #[test]
    fn parse_seq() {
        assert_eq!(parse_sequence("1, 2.5, -3"), vec![1.0, 2.5, -3.0]);
        assert_eq!(parse_sequence("10 20;30"), vec![10.0, 20.0, 30.0]);
        assert_eq!(parse_sequence(" , 4 ,, 5 "), vec![4.0, 5.0]);
    }
}
