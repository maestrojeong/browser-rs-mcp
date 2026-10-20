//! Human pointer travel from recorded-style motion templates.
//!
//! `assets/mime_templates.json` holds 387 trajectories generated offline with
//! the MIME-mouse model (<https://github.com/4LAU/MIME-mouse>, Apache-2.0) and
//! filtered for clean shape (no teleport-sized steps, bounded lateral drift
//! and overshoot, no backtracking, lands on target). See
//! `assets/generate_mime_pool.py` and `assets/build_mime_templates.py`.
//!
//! Each template is stored in its own start->target frame:
//! `[angle_rad, distance_px, [[dt_ms, along/D, lateral/D], ...]]`.
//!
//! A travel plan picks one of the nearest templates by direction and log
//! distance, maps it onto the requested segment, and applies weak variation:
//! lateral gain U(0.2, 0.4), global time scale (D/D0)^0.553 x lognormal(0.1),
//! and a per-trajectory probability U(0.6, 1.0) of keeping 0px stall events.
//! Event spacing and displacement come from the same template, so speed and
//! path stay coupled the way the source motion was.

use rand::Rng;
use std::sync::OnceLock;

type Event = (f64, f64, f64);
type Template = (f64, f64, Vec<Event>);

const NEAREST: usize = 8;
const ANGLE_SCALE: f64 = 0.35;
const LOG_DIST_SCALE: f64 = 0.5;
const DURATION_EXPONENT: f64 = 0.553;

fn templates() -> &'static [Template] {
    static POOL: OnceLock<Vec<Template>> = OnceLock::new();
    POOL.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/mime_templates.json"))
            .expect("embedded mime_templates.json is valid")
    })
}

/// One pointer event: absolute position and the delay to wait *before* it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MotionStep {
    pub x: f64,
    pub y: f64,
    pub delay_ms: f64,
}

fn angle_diff(a: f64, b: f64) -> f64 {
    let d = (a - b).abs() % std::f64::consts::TAU;
    if d > std::f64::consts::PI {
        std::f64::consts::TAU - d
    } else {
        d
    }
}

fn normal<R: Rng + ?Sized>(rng: &mut R, sd: f64) -> f64 {
    // Box-Muller; avoids pulling rand_distr into this module's signature.
    let u1: f64 = rng.gen_range(f64::EPSILON..1.0);
    let u2: f64 = rng.gen::<f64>();
    sd * (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// Plan a travel from (sx, sy) to (x, y). Returns `None` when the segment is
/// shorter than 2px (callers should fall back to a direct move).
pub(crate) fn plan_travel<R: Rng + ?Sized>(
    sx: f64,
    sy: f64,
    x: f64,
    y: f64,
    rng: &mut R,
) -> Option<Vec<MotionStep>> {
    let dx = x - sx;
    let dy = y - sy;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 2.0 {
        return None;
    }
    let angle = dy.atan2(dx);
    let (ux, uy) = (dx / dist, dy / dist);
    let (nx, ny) = (-uy, ux);

    let pool = templates();
    let mut scored: Vec<(f64, usize)> = pool
        .iter()
        .enumerate()
        .map(|(i, (a0, d0, _))| {
            let da = angle_diff(*a0, angle) / ANGLE_SCALE;
            let dl = (dist / d0).ln() / LOG_DIST_SCALE;
            (da * da + dl * dl, i)
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    let pick = scored[rng.gen_range(0..NEAREST.min(scored.len()))].1;
    let (_, d0, events) = &pool[pick];

    let lateral_gain = rng.gen_range(0.2..0.4);
    let keep_stall = rng.gen_range(0.6..1.0);
    let time_scale = (dist / d0).powf(DURATION_EXPONENT) * normal(rng, 0.1).exp();

    let (rx, ry) = (sx.round(), sy.round());
    let mut last = (rx, ry);
    let mut pending = 0.0;
    let mut steps = Vec::with_capacity(events.len() + 1);
    for &(dt, along, lateral) in events {
        pending += dt * time_scale;
        let s = along * dist;
        let l = lateral * dist * lateral_gain;
        let px = (sx + ux * s + nx * l).round();
        let py = (sy + uy * s + ny * l).round();
        if (px, py) == last && !steps.is_empty() && rng.gen::<f64>() > keep_stall {
            continue; // merge this stall into the next event's delay
        }
        steps.push(MotionStep {
            x: px,
            y: py,
            delay_ms: pending,
        });
        pending = 0.0;
        last = (px, py);
    }
    // The final move lands on the exact requested point (not its rounding):
    // callers press the button at (x, y), and a press that doesn't coincide
    // with the last mouseMoved is a synthetic tell.
    match steps.last_mut() {
        Some(last_step) if (last_step.x, last_step.y) == (x.round(), y.round()) => {
            last_step.x = x;
            last_step.y = y;
        }
        _ => steps.push(MotionStep {
            x,
            y,
            delay_ms: pending + rng.gen_range(5.0..11.0),
        }),
    }
    Some(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn templates_parse() {
        assert!(templates().len() > 300);
        assert!(templates().iter().all(|t| !t.2.is_empty()));
    }

    #[test]
    fn plan_lands_on_target_without_teleports() {
        let mut rng = StdRng::seed_from_u64(7);
        for i in 0..400 {
            let ang = i as f64 * 0.37;
            let dist = 40.0 + (i % 20) as f64 * 45.0;
            let (sx, sy) = (900.0, 500.0);
            let (x, y) = (sx + dist * ang.cos(), sy + dist * ang.sin());
            let steps = plan_travel(sx, sy, x, y, &mut rng).unwrap();
            let end = steps.last().unwrap();
            assert_eq!((end.x, end.y), (x, y));
            let mut prev = (sx.round(), sy.round());
            let mut total = 0.0;
            for s in &steps {
                let jump = ((s.x - prev.0).powi(2) + (s.y - prev.1).powi(2)).sqrt();
                assert!(
                    jump <= (dist * 0.16).max(10.0),
                    "jump {jump} at dist {dist}"
                );
                assert!(s.delay_ms >= 0.0);
                total += s.delay_ms;
                prev = (s.x, s.y);
            }
            assert!(total > 30.0 && total < 6000.0, "duration {total}");
        }
    }

    #[test]
    fn short_segment_falls_back() {
        let mut rng = StdRng::seed_from_u64(1);
        assert!(plan_travel(10.0, 10.0, 11.0, 10.0, &mut rng).is_none());
    }
}
