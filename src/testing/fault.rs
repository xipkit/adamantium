//! Deterministic error and latency injection for mocked async traits.
//!
//! [`FaultInjector`] lets mocked storage, HTTP, and RPC traits inject
//! replayable errors (by probability, call-count rule, or scripted schedule)
//! and virtual-clock latency without real timers or non-deterministic RNGs.
//!
//! Every random choice uses [`crate::random`] and every delay uses
//! [`crate::time::sleep`], so executions under [`crate::testing::check`] and
//! [`crate::testing::replay`] reproduce identically across reruns.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::Arc,
    time::Duration,
};

use crate::runtime::{Lock, Shared};

type ErrorFn<E> = Arc<dyn Fn() -> E + Send + Sync>;
type PredicateFn = Arc<dyn Fn(usize) -> bool + Send + Sync>;

/// Virtual-clock delay applied by a [`FaultRule`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Latency {
    /// No delay.
    #[default]
    None,
    /// A constant virtual-clock delay.
    Fixed(Duration),
    /// A uniformly distributed virtual-clock delay in `[min, max]`.
    Range {
        /// Minimum delay, inclusive.
        min: Duration,
        /// Maximum delay, inclusive.
        max: Duration,
    },
}

impl Latency {
    /// A constant virtual-clock delay.
    pub fn fixed(duration: Duration) -> Self {
        if duration.is_zero() {
            Self::None
        } else {
            Self::Fixed(duration)
        }
    }

    /// A uniformly distributed virtual-clock delay in `[min, max]`.
    ///
    /// # Panics
    ///
    /// Panics if `min > max`.
    pub fn range(min: Duration, max: Duration) -> Self {
        assert!(
            min <= max,
            "adamantium::testing::Latency::range requires min <= max"
        );
        if min == max {
            Self::fixed(min)
        } else {
            Self::Range { min, max }
        }
    }

    fn sample(self) -> Duration {
        match self {
            Self::None => Duration::ZERO,
            Self::Fixed(duration) => duration,
            Self::Range { min, max } => {
                if min == max {
                    return min;
                }
                let span = max.saturating_sub(min);
                if let Ok(nanos) = u64::try_from(span.as_nanos()) {
                    min + Duration::from_nanos(crate::random::up_to(nanos))
                } else {
                    let millis = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
                    min + Duration::from_millis(crate::random::up_to(millis))
                }
            }
        }
    }
}

/// A probabilistic or predicate-driven fault and latency rule for [`FaultInjector`].
pub struct FaultRule<E = ()> {
    latency: Latency,
    error_probability: f64,
    condition: Option<PredicateFn>,
    make_error: Option<ErrorFn<E>>,
}

impl<E> Clone for FaultRule<E> {
    fn clone(&self) -> Self {
        Self {
            latency: self.latency,
            error_probability: self.error_probability,
            condition: self.condition.clone(),
            make_error: self.make_error.clone(),
        }
    }
}

impl<E> fmt::Debug for FaultRule<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultRule")
            .field("latency", &self.latency)
            .field("error_probability", &self.error_probability)
            .field("has_condition", &self.condition.is_some())
            .field("has_error", &self.make_error.is_some())
            .finish()
    }
}

impl<E> Default for FaultRule<E> {
    fn default() -> Self {
        Self::ok()
    }
}

impl<E> FaultRule<E> {
    /// A rule that injects neither errors nor latency.
    pub fn ok() -> Self {
        Self {
            latency: Latency::None,
            error_probability: 0.0,
            condition: None,
            make_error: None,
        }
    }

    /// A rule that injects `latency` without errors.
    pub fn delay(latency: Latency) -> Self {
        Self::ok().with_latency(latency)
    }

    /// A rule that fails with `error` at `probability` in `0.0..=1.0`.
    ///
    /// # Panics
    ///
    /// Panics if `probability` is outside `0.0..=1.0` or `NaN`.
    pub fn fail(probability: f64, error: E) -> Self
    where
        E: Clone + Send + Sync + 'static,
    {
        Self::ok().with_error(probability, error)
    }

    /// A rule that fails using `make_error` at `probability` in `0.0..=1.0`.
    ///
    /// # Panics
    ///
    /// Panics if `probability` is outside `0.0..=1.0` or `NaN`.
    pub fn fail_with(probability: f64, make_error: impl Fn() -> E + Send + Sync + 'static) -> Self {
        Self::ok().with_error_fn(probability, make_error)
    }

    /// A rule that fails with `error` whenever `predicate(call_number)` is true.
    ///
    /// `call_number` is 1-based for the matching operation.
    pub fn fail_when(predicate: impl Fn(usize) -> bool + Send + Sync + 'static, error: E) -> Self
    where
        E: Clone + Send + Sync + 'static,
    {
        Self::ok().with_condition(predicate).with_error(1.0, error)
    }

    /// A rule that fails using `make_error` whenever `predicate(call_number)` is true.
    ///
    /// `call_number` is 1-based for the matching operation.
    pub fn fail_when_with(
        predicate: impl Fn(usize) -> bool + Send + Sync + 'static,
        make_error: impl Fn() -> E + Send + Sync + 'static,
    ) -> Self {
        Self::ok()
            .with_condition(predicate)
            .with_error_fn(1.0, make_error)
    }

    /// Sets the virtual-clock latency for this rule.
    pub fn with_latency(mut self, latency: Latency) -> Self {
        self.latency = latency;
        self
    }

    /// Sets a constant virtual-clock latency for this rule.
    pub fn with_fixed_latency(self, duration: Duration) -> Self {
        self.with_latency(Latency::fixed(duration))
    }

    /// Sets a uniform random virtual-clock latency in `[min, max]`.
    ///
    /// # Panics
    ///
    /// Panics if `min > max`.
    pub fn with_latency_range(self, min: Duration, max: Duration) -> Self {
        self.with_latency(Latency::range(min, max))
    }

    /// Sets the error probability in `0.0..=1.0` and error value.
    ///
    /// # Panics
    ///
    /// Panics if `probability` is outside `0.0..=1.0` or `NaN`.
    pub fn with_error(self, probability: f64, error: E) -> Self
    where
        E: Clone + Send + Sync + 'static,
    {
        self.with_error_fn(probability, move || error.clone())
    }

    /// Sets the error probability in `0.0..=1.0` and error factory.
    ///
    /// # Panics
    ///
    /// Panics if `probability` is outside `0.0..=1.0` or `NaN`.
    pub fn with_error_fn(
        mut self,
        probability: f64,
        make_error: impl Fn() -> E + Send + Sync + 'static,
    ) -> Self {
        assert!(
            (0.0..=1.0).contains(&probability),
            "adamantium::testing::FaultRule probability must be in 0.0..=1.0"
        );
        self.error_probability = probability;
        self.make_error = Some(Arc::new(make_error));
        self
    }

    /// Restricts error injection to calls where `predicate(call_number)` returns `true`.
    ///
    /// `call_number` is 1-based for the matching operation.
    pub fn with_condition(
        mut self,
        predicate: impl Fn(usize) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.condition = Some(Arc::new(predicate));
        self
    }

    fn evaluate(&self, op_call: usize) -> (Duration, Option<ErrorFn<E>>) {
        let delay = self.latency.sample();
        let make_error = self.make_error.as_ref().and_then(|factory| {
            let condition_met = self
                .condition
                .as_ref()
                .is_none_or(|predicate| predicate(op_call));
            if condition_met && sample_probability(self.error_probability) {
                Some(Arc::clone(factory))
            } else {
                None
            }
        });
        (delay, make_error)
    }
}

/// One scripted step in a [`FaultInjector`] schedule.
pub struct FaultStep<E = ()> {
    latency: Duration,
    make_error: Option<ErrorFn<E>>,
}

impl<E> Clone for FaultStep<E> {
    fn clone(&self) -> Self {
        Self {
            latency: self.latency,
            make_error: self.make_error.clone(),
        }
    }
}

impl<E> fmt::Debug for FaultStep<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultStep")
            .field("latency", &self.latency)
            .field("has_error", &self.make_error.is_some())
            .finish()
    }
}

impl<E> FaultStep<E> {
    /// Succeeds immediately.
    pub fn ok() -> Self {
        Self {
            latency: Duration::ZERO,
            make_error: None,
        }
    }

    /// Waits for `duration` on the virtual clock, then succeeds.
    pub fn delay(duration: Duration) -> Self {
        Self {
            latency: duration,
            make_error: None,
        }
    }

    /// Fails immediately with `error`.
    pub fn fail(error: E) -> Self
    where
        E: Clone + Send + Sync + 'static,
    {
        Self::fail_with(move || error.clone())
    }

    /// Fails immediately with the error returned by `make_error`.
    pub fn fail_with(make_error: impl Fn() -> E + Send + Sync + 'static) -> Self {
        Self {
            latency: Duration::ZERO,
            make_error: Some(Arc::new(make_error)),
        }
    }

    /// Waits for `duration` on the virtual clock, then fails with `error`.
    pub fn delay_then_fail(duration: Duration, error: E) -> Self
    where
        E: Clone + Send + Sync + 'static,
    {
        Self::delay_then_fail_with(duration, move || error.clone())
    }

    /// Waits for `duration` on the virtual clock, then fails using `make_error`.
    pub fn delay_then_fail_with(
        duration: Duration,
        make_error: impl Fn() -> E + Send + Sync + 'static,
    ) -> Self {
        Self {
            latency: duration,
            make_error: Some(Arc::new(make_error)),
        }
    }
}

struct State<E> {
    default_rule: Option<FaultRule<E>>,
    rules: BTreeMap<String, FaultRule<E>>,
    global_schedule: VecDeque<FaultStep<E>>,
    op_schedules: BTreeMap<String, VecDeque<FaultStep<E>>>,
    op_calls: BTreeMap<String, usize>,
    total_calls: usize,
}

impl<E> Default for State<E> {
    fn default() -> Self {
        Self {
            default_rule: None,
            rules: BTreeMap::new(),
            global_schedule: VecDeque::new(),
            op_schedules: BTreeMap::new(),
            op_calls: BTreeMap::new(),
            total_calls: 0,
        }
    }
}

enum Decision<E> {
    Scripted(FaultStep<E>),
    Rule(FaultRule<E>, usize),
    Pass,
}

/// Deterministic error and virtual-clock latency injector for mocked async traits.
///
/// Cloneable handles share the same rules, scripted queues, and call counters
/// within the execution that created them.
pub struct FaultInjector<E = ()> {
    state: Shared<Lock<State<E>>>,
}

impl<E> Clone for FaultInjector<E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone_handle("FaultInjector::clone"),
        }
    }
}

impl<E> fmt::Debug for FaultInjector<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultInjector").finish_non_exhaustive()
    }
}

impl<E> Default for FaultInjector<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> FaultInjector<E> {
    /// Creates an injector with no rules or scheduled faults.
    pub fn new() -> Self {
        Self {
            state: Shared::new(Lock::new(State::default()), "FaultInjector::drop"),
        }
    }

    /// Sets the fallback rule used when no operation-specific rule matches.
    pub fn with_default_rule(self, rule: FaultRule<E>) -> Self {
        self.set_default_rule(rule);
        self
    }

    /// Sets or replaces the fallback rule used when no operation-specific rule matches.
    pub fn set_default_rule(&self, rule: FaultRule<E>) {
        self.state
            .lock("FaultInjector::set_default_rule")
            .default_rule = Some(rule);
    }

    /// Clears the fallback rule.
    pub fn clear_default_rule(&self) {
        self.state
            .lock("FaultInjector::clear_default_rule")
            .default_rule = None;
    }

    /// Sets the rule for `op`, returning `self` for chaining.
    pub fn with_rule(self, op: impl Into<String>, rule: FaultRule<E>) -> Self {
        self.set_rule(op, rule);
        self
    }

    /// Sets or replaces the rule for `op`.
    pub fn set_rule(&self, op: impl Into<String>, rule: FaultRule<E>) {
        self.state
            .lock("FaultInjector::set_rule")
            .rules
            .insert(op.into(), rule);
    }

    /// Removes any rule registered for `op`.
    pub fn clear_rule(&self, op: &str) {
        self.state
            .lock("FaultInjector::clear_rule")
            .rules
            .remove(op);
    }

    /// Appends scripted steps to the global schedule, returning `self` for chaining.
    pub fn with_schedule(self, steps: impl IntoIterator<Item = FaultStep<E>>) -> Self {
        self.push_schedule(steps);
        self
    }

    /// Appends scripted steps to the global schedule.
    pub fn push_schedule(&self, steps: impl IntoIterator<Item = FaultStep<E>>) {
        self.state
            .lock("FaultInjector::push_schedule")
            .global_schedule
            .extend(steps);
    }

    /// Appends scripted steps for `op`, returning `self` for chaining.
    pub fn with_op_schedule(
        self,
        op: impl Into<String>,
        steps: impl IntoIterator<Item = FaultStep<E>>,
    ) -> Self {
        self.push_op_schedule(op, steps);
        self
    }

    /// Appends scripted steps for `op`.
    ///
    /// Operation-specific scripted steps are consumed before the global schedule
    /// and before rules.
    pub fn push_op_schedule(
        &self,
        op: impl Into<String>,
        steps: impl IntoIterator<Item = FaultStep<E>>,
    ) {
        self.state
            .lock("FaultInjector::push_op_schedule")
            .op_schedules
            .entry(op.into())
            .or_default()
            .extend(steps);
    }

    /// Injects any scheduled or rule-driven latency and error for `op`.
    ///
    /// Precedence for each call:
    /// 1. Next [`FaultStep`] queued for `op`, if any.
    /// 2. Next [`FaultStep`] queued in the global schedule, if any.
    /// 3. The [`FaultRule`] registered for `op`, or the default rule.
    /// 4. Immediate `Ok(())`.
    pub async fn inject(&self, op: &str) -> Result<(), E> {
        let decision = {
            let mut state = self.state.lock("FaultInjector::inject");
            state.total_calls += 1;
            let op_call = if let Some(count) = state.op_calls.get_mut(op) {
                *count += 1;
                *count
            } else {
                state.op_calls.insert(op.to_owned(), 1);
                1
            };

            if let Some(step) = state.op_schedules.get_mut(op).and_then(VecDeque::pop_front) {
                Decision::Scripted(step)
            } else if let Some(step) = state.global_schedule.pop_front() {
                Decision::Scripted(step)
            } else if let Some(rule) = state.rules.get(op).or(state.default_rule.as_ref()) {
                Decision::Rule(rule.clone(), op_call)
            } else {
                Decision::Pass
            }
        };

        let (delay, make_error) = match decision {
            Decision::Scripted(step) => (step.latency, step.make_error),
            Decision::Rule(rule, op_call) => rule.evaluate(op_call),
            Decision::Pass => (Duration::ZERO, None),
        };

        if !delay.is_zero() {
            crate::time::sleep(delay).await;
        }

        if let Some(make_error) = make_error {
            Err(make_error())
        } else {
            Ok(())
        }
    }

    /// Returns how many times `inject(op)` has been called.
    pub fn calls(&self, op: &str) -> usize {
        self.state
            .lock("FaultInjector::calls")
            .op_calls
            .get(op)
            .copied()
            .unwrap_or(0)
    }

    /// Returns the total number of `inject` calls across all operations.
    pub fn total_calls(&self) -> usize {
        self.state.lock("FaultInjector::total_calls").total_calls
    }
}

fn sample_probability(probability: f64) -> bool {
    if probability <= 0.0 {
        return false;
    }
    if probability >= 1.0 {
        return true;
    }
    // Shift to 53 uniform bits so the divisor is exact in IEEE 754 f64.
    let unit = (crate::random::u64() >> 11) as f64 / ((1_u64 << 53) as f64);
    unit < probability
}
