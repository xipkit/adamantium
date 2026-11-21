//! Tracing's callsite cache is shared by the whole process, so a callsite that
//! another test reaches first, under no subscriber, can stay disabled for this
//! one. This test has its process to itself.
#![cfg(feature = "deterministic")]
#![allow(
    clippy::disallowed_types,
    reason = "synchronous test subscriber state; locks never cross awaits"
)]

use std::sync::{Arc, Mutex};

use adamantium::testing::{self, Config};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

/// Records the outermost span around each event this file emits.
#[derive(Clone, Default)]
struct OutermostSpans(Arc<Mutex<Vec<Option<&'static str>>>>);

impl<S> Layer<S> for OutermostSpans
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        if event.metadata().target() != module_path!() {
            return;
        }
        let outermost = context
            .event_scope(event)
            .and_then(|scope| scope.from_root().next())
            .map(|span| span.name());
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(outermost);
    }
}

#[test]
fn scenarios_report_to_the_callers_tracing_subscriber_inside_its_span() {
    let spans = OutermostSpans::default();
    let subscriber = tracing_subscriber::registry().with(spans.clone());
    tracing::subscriber::with_default(subscriber, || {
        let _caller = tracing::info_span!("caller").entered();
        let config = Config {
            iterations: 2,
            ..Config::default()
        };
        testing::check(config, || async { tracing::info!("in the scenario") }).unwrap();
    });
    assert_eq!(*spans.0.lock().unwrap(), [Some("caller"), Some("caller")]);
}
