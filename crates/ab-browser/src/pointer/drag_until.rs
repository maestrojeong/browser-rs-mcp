//! Closed-loop drag: press, move a little, look at the page, repeat.
//!
//! A plain drag is fire-and-forget (press → one curved path → release). Some
//! targets cannot be reached that way because the correct release point is
//! only visible while the pointer is moving (e.g. a slider whose response is
//! non-linear). `drag_until` keeps the button pressed and moves in small hops,
//! evaluating a page expression after every hop, until the expression says
//! "stop". The button is released on every exit path.

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{PointerAction, PointerLocation, PointerRequest};
use crate::{rand_f64, sample_lognormal_ms, sample_normal, BrowserError, Page, Result};

/// How the page expression is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntilMode {
    /// Stop at the first hop where the expression is true (boolean) or
    /// `>= threshold` (number).
    True,
    /// Sweep the whole range, then return to the position with the highest
    /// numeric score and release there.
    Max,
}

impl UntilMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "true" => Ok(Self::True),
            "max" => Ok(Self::Max),
            other => Err(BrowserError::Protocol(format!(
                "unknown until_mode {other:?} (use \"true\" or \"max\")"
            ))),
        }
    }
}

/// Where the drag may travel.
#[derive(Debug, Clone)]
pub enum DragRange {
    /// From the origin to this location.
    To(PointerLocation),
    /// From the origin along a unit vector for at most `distance` px.
    Along { dx: f64, dy: f64, distance: f64 },
}

#[derive(Debug, Clone)]
pub struct DragUntil {
    /// JS expression evaluated in the page after each hop.
    pub expression: String,
    pub mode: UntilMode,
    /// For numeric expressions in `True` mode: satisfied when score >= threshold.
    pub threshold: Option<f64>,
    pub step_px: f64,
    pub settle_ms: u64,
    /// The condition must hold, pointer still, for this long before releasing.
    pub dwell_ms: u64,
    /// Hard cap for the whole loop.
    pub max_ms: u64,
}

#[derive(Debug, Clone)]
pub struct DragUntilRequest {
    pub origin: PointerLocation,
    pub range: DragRange,
    pub until: DragUntil,
}

#[derive(Debug, Clone, Serialize)]
pub struct DragUntilOutcome {
    pub trusted: bool,
    pub condition_met: bool,
    /// `condition_met`, `best_of_sweep`, `range_end`, or `timeout`.
    pub reason: &'static str,
    pub steps: usize,
    pub elapsed_ms: u64,
    /// Distance travelled along the range when the button was released.
    pub released_at_px: f64,
    pub final_score: Option<f64>,
    pub best_score: Option<f64>,
}

/// One reading of the page expression.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Reading {
    pub met: bool,
    pub score: f64,
    /// The expression returned a number (a score), not a boolean.
    pub numeric: bool,
}

/// Turn the expression's JSON result into a [`Reading`].
pub(super) fn interpret(value: &Value, until: &DragUntil) -> Result<Reading> {
    match value {
        Value::Bool(b) => {
            if until.mode == UntilMode::Max {
                return Err(BrowserError::Protocol(
                    "until_mode \"max\" needs a numeric expression, got a boolean".into(),
                ));
            }
            Ok(Reading {
                met: *b,
                score: if *b { 1.0 } else { 0.0 },
                numeric: false,
            })
        }
        Value::Number(n) => {
            let score = n.as_f64().filter(|s| s.is_finite()).ok_or_else(|| {
                BrowserError::Protocol("until expression returned a non-finite number".into())
            })?;
            let met = match (until.mode, until.threshold) {
                (UntilMode::True, Some(t)) => score >= t,
                (UntilMode::True, None) => {
                    return Err(BrowserError::Protocol(
                        "a numeric until expression needs `threshold` in \"true\" mode".into(),
                    ))
                }
                (UntilMode::Max, Some(t)) => score >= t,
                (UntilMode::Max, None) => false,
            };
            Ok(Reading {
                met,
                score,
                numeric: true,
            })
        }
        other => Err(BrowserError::Protocol(format!(
            "until expression must return a boolean or a number, got {other}"
        ))),
    }
}

/// Distances (px from the origin) visited on the way out, ending exactly at
/// `length`. Hops vary a little in size so the cadence is not metronomic.
pub(super) fn plan_hops(length: f64, step_px: f64, jitter: impl Fn() -> f64) -> Vec<f64> {
    let step = step_px.max(1.0);
    let mut at = 0.0;
    let mut out = Vec::new();
    while at < length - 1e-9 {
        let next = (at + step * (0.75 + 0.5 * jitter().clamp(0.0, 1.0))).min(length);
        out.push(next);
        at = next;
    }
    out
}

struct LoopState {
    cur: (f64, f64),
    lateral: f64,
    steps: usize,
    started: Instant,
    last: Option<Reading>,
    best_score: Option<f64>,
}

struct CancelDragOnDrop(Option<CancellationToken>);

impl CancelDragOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelDragOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel.cancel();
        }
    }
}

impl Page {
    pub async fn drag_until(&self, request: &DragUntilRequest) -> Result<DragUntilOutcome> {
        validate_until(&request.until)?;
        if let DragRange::Along { dx, dy, distance } = &request.range {
            if !(dx.is_finite() && dy.is_finite() && distance.is_finite() && *distance > 0.0) {
                return Err(BrowserError::Protocol(
                    "drag range must have a finite, positive distance".into(),
                ));
            }
        }
        let cancel = CancellationToken::new();
        let mut cancel_on_drop = CancelDragOnDrop(Some(cancel.clone()));
        let page = self.clone();
        let request = request.clone();
        let task = tokio::spawn(async move { page.drag_until_owned(&request, cancel).await });
        let result = task.await.map_err(|error| {
            BrowserError::Protocol(format!("closed-loop drag task failed: {error}"))
        })?;
        cancel_on_drop.disarm();
        result
    }

    async fn drag_until_owned(
        &self,
        request: &DragUntilRequest,
        cancel: CancellationToken,
    ) -> Result<DragUntilOutcome> {
        let _mutation = self.pointer_mutation.lock().await;
        if cancel.is_cancelled() {
            return Err(BrowserError::Protocol("drag cancelled".into()));
        }
        self.validate_pointer_refs(&PointerRequest {
            action: PointerAction::Drag,
            origin: request.origin.clone(),
            destination: match &request.range {
                DragRange::To(location) => Some(location.clone()),
                DragRange::Along { .. } => None,
            },
            delta_x: 0.0,
            delta_y: 0.0,
        })
        .await?;

        if let PointerLocation::Element(element) = &request.origin {
            self.scroll_into_view(element.backend_node_id).await;
        }
        if let DragRange::To(PointerLocation::Element(element)) = &request.range {
            self.scroll_into_view(element.backend_node_id).await;
        }
        let origin = self.point_without_scroll(&request.origin).await?;
        self.require_visible_hit(&request.origin, (origin.0, origin.1))
            .await?;
        let end = match &request.range {
            DragRange::To(location) => {
                let point = self.point_without_scroll(location).await?;
                self.require_visible_hit(location, (point.0, point.1))
                    .await?;
                (point.0, point.1)
            }
            DragRange::Along { dx, dy, distance } => {
                let norm = (dx * dx + dy * dy).sqrt().max(1e-9);
                (
                    origin.0 + dx / norm * distance,
                    origin.1 + dy / norm * distance,
                )
            }
        };
        let (ox, oy) = (origin.0, origin.1);
        let length = ((end.0 - ox).powi(2) + (end.1 - oy).powi(2)).sqrt();
        if !(ox.is_finite()
            && oy.is_finite()
            && end.0.is_finite()
            && end.1.is_finite()
            && length.is_finite())
        {
            return Err(BrowserError::Protocol(
                "drag coordinates must be finite".into(),
            ));
        }
        if length < 1.0 {
            return Err(BrowserError::Protocol(
                "drag range is shorter than 1 px".into(),
            ));
        }

        self.human_move_to(ox, oy, origin.2).await?;
        if cancel.is_cancelled() {
            return Err(BrowserError::Protocol("drag cancelled".into()));
        }
        self.require_click_target_after_move(&request.origin, (ox, oy))
            .await?;
        if cancel.is_cancelled() {
            return Err(BrowserError::Protocol("drag cancelled".into()));
        }
        self.mouse_button("mousePressed", ox, oy, "left", 1, 1)
            .await?;

        let started = Instant::now();
        let deadline = started + Duration::from_millis(request.until.max_ms);
        let mut state = LoopState {
            cur: (ox, oy),
            lateral: 0.0,
            steps: 0,
            started,
            last: None,
            best_score: None,
        };
        let looped = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(BrowserError::Protocol("drag cancelled".into())),
            result = tokio::time::timeout_at(
                deadline,
                self.drag_until_loop(
                    &mut state,
                    (ox, oy),
                    end,
                    length,
                    &request.until,
                    deadline,
                ),
            ) => match result {
                Ok(result) => result,
                Err(_) => Ok(outcome("timeout", false, &state)),
            },
        };
        // Always let go, even after an error, so the button is never left down.
        let release = self
            .mouse_button("mouseReleased", state.cur.0, state.cur.1, "left", 0, 1)
            .await;
        *self.pointer.lock().unwrap() = Some(state.cur);
        let mut outcome = looped?;
        release?;
        outcome.steps = state.steps;
        outcome.elapsed_ms = state.started.elapsed().as_millis() as u64;
        outcome.released_at_px = ((state.cur.0 - ox).powi(2) + (state.cur.1 - oy).powi(2)).sqrt();
        Ok(outcome)
    }

    async fn drag_until_loop(
        &self,
        state: &mut LoopState,
        origin: (f64, f64),
        end: (f64, f64),
        length: f64,
        until: &DragUntil,
        deadline: Instant,
    ) -> Result<DragUntilOutcome> {
        let point_at = |s: f64| {
            let t = s / length;
            (
                origin.0 + (end.0 - origin.0) * t,
                origin.1 + (end.1 - origin.1) * t,
            )
        };
        let mut travelled = 0.0;
        let mut samples: Vec<(f64, f64)> = Vec::new(); // (distance, score)

        for target in plan_hops(length, until.step_px, || rand_f64(0.5) + 0.5) {
            if Instant::now() >= deadline {
                return Ok(outcome("timeout", false, state));
            }
            self.drag_hop(state, point_at(target), (origin, end))
                .await?;
            travelled = target;
            let reading = self.read_until(until).await?;
            state.steps += 1;
            state.last = Some(reading);
            state.best_score = Some(
                state
                    .best_score
                    .map_or(reading.score, |best| best.max(reading.score)),
            );
            samples.push((travelled, reading.score));
            // if it slips during the dwell we simply keep going
            if until.mode == UntilMode::True
                && reading.met
                && self.hold_condition(until, deadline).await?
            {
                return Ok(outcome("condition_met", true, state));
            }
        }

        let numeric = state.last.is_some_and(|r| r.numeric);
        if until.mode == UntilMode::True && !numeric {
            return Ok(outcome("range_end", false, state));
        }

        // Numeric scores: the coarse sweep can hop straight over a narrow peak, so go back to
        // just before the best position and re-scan it 1 px at a time.
        let (best_at, best) = samples
            .iter()
            .copied()
            .fold(
                (0.0, f64::NEG_INFINITY),
                |acc, s| if s.1 > acc.1 { s } else { acc },
            );
        let step = until.step_px.max(1.0);
        let lo = (best_at - step).max(0.0);
        let hi = (best_at + step).min(travelled);
        let mut back = travelled;
        while back > lo + 1e-9 {
            back = (back - step * (0.75 + 0.5 * rand_f64(0.5).abs())).max(lo);
            self.drag_hop(state, point_at(back), (origin, end)).await?;
            state.steps += 1;
        }
        let mut fine_best = (lo, f64::NEG_INFINITY);
        let mut at = lo;
        let mut fine_last;
        loop {
            let reading = self.read_until(until).await?;
            state.steps += 1;
            state.last = Some(reading);
            state.best_score = Some(
                state
                    .best_score
                    .map_or(reading.score, |best| best.max(reading.score)),
            );
            fine_last = Some(reading);
            if reading.score > fine_best.1 {
                fine_best = (at, reading.score);
            }
            if until.mode == UntilMode::True
                && reading.met
                && self.hold_condition(until, deadline).await?
            {
                return Ok(outcome("condition_met", true, state));
            }
            if at >= hi - 1e-9 || Instant::now() >= deadline {
                break;
            }
            at = (at + 1.0).min(hi);
            self.drag_hop(state, point_at(at), (origin, end)).await?;
        }
        // settle on the best position found and confirm it
        if (at - fine_best.0).abs() > 1e-9 {
            self.drag_hop(state, point_at(fine_best.0), (origin, end))
                .await?;
            state.steps += 1;
            fine_last = Some(self.read_until(until).await?);
            state.last = fine_last;
        }
        let best_overall = Some(best.max(fine_best.1));
        state.best_score = best_overall;
        let reading = fine_last.expect("fine scan reads at least once");
        if until.mode == UntilMode::True {
            return Ok(outcome("range_end", false, state));
        }
        let met = reading.met || until.threshold.is_none();
        let reason = if met { "best_of_sweep" } else { "range_end" };
        Ok(outcome(reason, met, state))
    }

    /// Re-check the condition every ~40 ms, pointer still, until it has held
    /// for `dwell_ms`. Returns false if it stopped holding.
    async fn hold_condition(&self, until: &DragUntil, deadline: Instant) -> Result<bool> {
        if until.dwell_ms == 0 {
            return Ok(true);
        }
        let held_from = Instant::now();
        while held_from.elapsed() < Duration::from_millis(until.dwell_ms) {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let remaining =
                Duration::from_millis(until.dwell_ms).saturating_sub(held_from.elapsed());
            tokio::time::sleep(remaining.min(Duration::from_millis(40))).await;
            if !self.read_until(until).await?.met {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn read_until(&self, until: &DragUntil) -> Result<Reading> {
        if until.settle_ms > 0 {
            tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(
                until.settle_ms as f64,
                0.2,
                1,
                2_000,
            )))
            .await;
        }
        let value = self.evaluate(&until.expression).await?;
        interpret(&value, until)
    }

    /// One short pressed-button hop with a few `mouseMoved` events. Motion is
    /// continuous across hops (no per-hop ease), with a small drifting
    /// sideways wobble perpendicular to the drag axis.
    async fn drag_hop(
        &self,
        state: &mut LoopState,
        to: (f64, f64),
        axis: ((f64, f64), (f64, f64)),
    ) -> Result<()> {
        let (sx, sy) = state.cur;
        let (ax, ay) = (axis.1 .0 - axis.0 .0, axis.1 .1 - axis.0 .1);
        let alen = (ax * ax + ay * ay).sqrt().max(1e-9);
        let (nx, ny) = (-ay / alen, ax / alen);
        let dist = ((to.0 - sx).powi(2) + (to.1 - sy).powi(2)).sqrt();
        // Tiny 1 px refinement hops should be one event, not a mechanically
        // repeated pair. Longer hops still get enough events to look continuous.
        let events = ((dist / 3.5).ceil() as usize).clamp(1, 8);
        for i in 1..=events {
            let u = i as f64 / events as f64;
            state.lateral = 0.85 * state.lateral + sample_normal(0.0, 0.25);
            let x = sx + (to.0 - sx) * u + nx * state.lateral;
            let y = sy + (to.1 - sy) * u + ny * state.lateral;
            self.client
                .send_on(
                    &self.session_id,
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseMoved", "x": x, "y": y, "button": "left", "buttons": 1 }),
                )
                .await?;
            state.cur = (x, y);
            tokio::time::sleep(Duration::from_millis(sample_lognormal_ms(9.0, 0.3, 3, 30))).await;
        }
        *self.pointer.lock().unwrap() = Some(state.cur);
        Ok(())
    }
}

fn outcome(reason: &'static str, met: bool, state: &LoopState) -> DragUntilOutcome {
    DragUntilOutcome {
        trusted: true,
        condition_met: met,
        reason,
        steps: 0,
        elapsed_ms: 0,
        released_at_px: 0.0,
        final_score: state.last.map(|reading| reading.score),
        best_score: state.best_score,
    }
}

fn validate_until(until: &DragUntil) -> Result<()> {
    if until.expression.trim().is_empty() {
        return Err(BrowserError::Protocol("until_js must not be empty".into()));
    }
    if !(until.step_px.is_finite() && until.step_px >= 1.0) {
        return Err(BrowserError::Protocol("step_px must be at least 1".into()));
    }
    if until.max_ms == 0 || until.max_ms > 120_000 {
        return Err(BrowserError::Protocol(
            "max_ms must be between 1 and 120000".into(),
        ));
    }
    if until.threshold.is_some_and(|t| !t.is_finite()) {
        return Err(BrowserError::Protocol("threshold must be finite".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn until(mode: UntilMode, threshold: Option<f64>) -> DragUntil {
        DragUntil {
            expression: "x".into(),
            mode,
            threshold,
            step_px: 6.0,
            settle_ms: 30,
            dwell_ms: 0,
            max_ms: 5_000,
        }
    }

    #[test]
    fn boolean_reading_in_true_mode() {
        let u = until(UntilMode::True, None);
        assert!(interpret(&json!(true), &u).unwrap().met);
        assert!(!interpret(&json!(false), &u).unwrap().met);
    }

    #[test]
    fn number_needs_threshold_in_true_mode() {
        assert!(interpret(&json!(0.9), &until(UntilMode::True, None)).is_err());
        let u = until(UntilMode::True, Some(0.95));
        assert!(!interpret(&json!(0.9), &u).unwrap().met);
        assert!(interpret(&json!(0.96), &u).unwrap().met);
    }

    #[test]
    fn max_mode_rejects_booleans_and_accepts_numbers() {
        let u = until(UntilMode::Max, None);
        assert!(interpret(&json!(true), &u).is_err());
        let reading = interpret(&json!(0.42), &u).unwrap();
        assert_eq!(reading.score, 0.42);
        assert!(!reading.met);
    }

    #[test]
    fn non_scalar_results_are_rejected() {
        let u = until(UntilMode::True, Some(0.5));
        assert!(interpret(&json!("yes"), &u).is_err());
        assert!(interpret(&Value::Null, &u).is_err());
    }

    #[test]
    fn hops_cover_the_range_and_end_exactly() {
        let hops = plan_hops(100.0, 6.0, || 0.5);
        assert_eq!(*hops.last().unwrap(), 100.0);
        assert!(hops.windows(2).all(|w| w[1] > w[0]));
        assert!(hops.len() >= 15 && hops.len() <= 25);
    }

    #[test]
    fn hop_jitter_changes_step_size() {
        let small = plan_hops(100.0, 10.0, || 0.0);
        let large = plan_hops(100.0, 10.0, || 1.0);
        assert!(small.len() > large.len());
    }

    #[test]
    fn one_hop_still_lands_exactly_on_a_short_range() {
        assert_eq!(plan_hops(3.0, 1000.0, || 1.0), vec![3.0]);
    }

    #[test]
    fn max_threshold_marks_only_high_enough_scores_as_met() {
        let u = until(UntilMode::Max, Some(-2.0));
        assert!(!interpret(&json!(-3.0), &u).unwrap().met);
        assert!(interpret(&json!(-1.0), &u).unwrap().met);
    }

    #[test]
    fn validation_rejects_bad_limits() {
        let mut u = until(UntilMode::True, None);
        assert!(validate_until(&u).is_ok());
        u.step_px = 0.5;
        assert!(validate_until(&u).is_err());
        u.step_px = 6.0;
        u.max_ms = 0;
        assert!(validate_until(&u).is_err());
        u.max_ms = 5_000;
        u.expression = "  ".into();
        assert!(validate_until(&u).is_err());
        u.expression = "x".into();
        u.step_px = f64::INFINITY;
        assert!(validate_until(&u).is_err());
        u.step_px = 6.0;
        u.threshold = Some(f64::NAN);
        assert!(validate_until(&u).is_err());
    }
}
