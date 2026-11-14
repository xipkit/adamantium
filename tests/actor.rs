use adamantium::Actor;
#[cfg(feature = "tokio")]
use adamantium::Tasks;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default, Clone, Debug, PartialEq)]
struct Tally {
    hits: u64,
}

impl Tally {
    fn record(&mut self) {
        self.hits += 1;
    }

    fn total(&self) -> u64 {
        self.hits
    }
}

struct LinkedState {
    next: Option<Actor<Self>>,
    drops: Arc<AtomicUsize>,
}

impl LinkedState {
    fn new(drops: &Arc<AtomicUsize>) -> Self {
        Self {
            next: None,
            drops: Arc::clone(drops),
        }
    }
}

impl Drop for LinkedState {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn acyclic_actor_links_drop_after_the_last_external_handle() {
    let drops = Arc::new(AtomicUsize::new(0));
    let first = Actor::new(LinkedState::new(&drops));
    let second = Actor::new(LinkedState::new(&drops));
    first.with(|state| state.next = Some(second.clone()));

    drop(second);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(first);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

#[test]
fn a_self_reference_keeps_actor_state_alive_without_external_handles() {
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = Actor::new(LinkedState::new(&drops));
    actor.with(|state| state.next = Some(actor.clone()));

    drop(actor);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
}

#[test]
fn an_indirect_cycle_keeps_actor_states_alive_without_external_handles() {
    let drops = Arc::new(AtomicUsize::new(0));
    let first = Actor::new(LinkedState::new(&drops));
    let second = Actor::new(LinkedState::new(&drops));
    first.with(|state| state.next = Some(second.clone()));
    second.with(|state| state.next = Some(first.clone()));

    drop(first);
    drop(second);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
}

#[test]
fn breaking_a_self_reference_allows_actor_state_to_drop() {
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = Actor::new(LinkedState::new(&drops));
    actor.with(|state| state.next = Some(actor.clone()));
    actor.with(|state| state.next = None);

    drop(actor);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[cfg(feature = "tokio")]
#[tokio::test(flavor = "multi_thread")]
async fn methods_run_one_at_a_time() {
    let tally = Actor::new(Tally::default());
    let result = Tasks::<&str>::run(async |t| {
        for _ in 0..8 {
            let tally = tally.clone();
            t.spawn(async move {
                for _ in 0..1000 {
                    tally.with(Tally::record);
                }
                Ok(())
            });
        }
        Ok(())
    })
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(tally.read(Tally::total), 8000);
    assert_eq!(tally.snapshot(), Tally { hits: 8000 });
}

#[test]
#[should_panic(expected = "inside another actor's method")]
fn a_method_cannot_call_another_actor() {
    let a = Actor::new(1);
    let b = Actor::new(2);
    a.with(|_| b.with(|_| ()));
}

#[test]
#[should_panic(expected = "inside another actor's method")]
fn a_read_cannot_nest_either() {
    let a = Actor::new(1);
    let b = Actor::new(2);
    a.read(|_| b.read(|_| ()));
}

#[test]
#[should_panic(expected = "inside another actor's method")]
fn a_method_cannot_reenter_its_own_actor() {
    let a = Actor::new(1);
    a.with(|_| a.with(|_| ()));
}

#[test]
fn a_panic_inside_a_method_does_not_poison_the_actor() {
    let a = Actor::new(Tally::default());
    let result = std::panic::catch_unwind(|| a.with(|_| panic!("mid-method")));
    assert!(result.is_err());
    a.with(Tally::record);
    assert_eq!(a.read(Tally::total), 1);
    let b = Actor::new(3);
    b.with(|n| *n += 1);
    assert_eq!(b.with(|n| *n), 4);
}

#[test]
fn into_inner_needs_the_only_handle() {
    let a = Actor::new(5);
    let b = a.clone();
    assert!(a.is_shared());
    let a = a.into_inner().unwrap_err();
    drop(b);
    assert_eq!(a.into_inner().ok(), Some(5));
}

#[test]
fn debug_shows_the_state_or_in_use() {
    let a = Actor::new(7);
    assert_eq!(format!("{a:?}"), "Actor(7)");
    a.with(|_| assert_eq!(format!("{a:?}"), "Actor(<in use>)"));
}
