use metal::Actor;

fn main() {
    let first = Actor::new(2_u8);
    let value = first.read(|state| *state);
    let second = Actor::new(0_u8);
    second.with(|state| *state = value + 1);
    assert_eq!(second.snapshot(), 3);
    println!("actor consumer: 3");
}
