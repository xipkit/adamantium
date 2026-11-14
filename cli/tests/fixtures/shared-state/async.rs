use std::sync::{Arc, Mutex};

struct Counter {
    hits: Arc<Mutex<u64>>,
}

impl Counter {
    async fn bump(&self) {
        tokio::task::yield_now().await;
        async {
            *self.hits.lock().unwrap() += 7;
        }.await;
    }

    async fn advance(&self) {
        self.bump().await;
    }

    fn value(&self) -> u64 {
        let value = *self.hits.lock().unwrap();
        value
    }
}

async fn local_leaf() -> u64 {
    let total = Arc::new(Mutex::new(0u64));
    let copy = total.clone();
    tokio::task::yield_now().await;
    async {
        *copy.lock().unwrap() += 11;
    }.await;
    let value = *total.lock().unwrap();
    value
}

async fn local_outer() -> u64 {
    local_leaf().await
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let counter = Counter { hits: Arc::new(Mutex::new(0)) };
    counter.advance().await;
    let field = counter.value();
    let local = async { local_outer().await }.await;
    println!("{field} {local}");
}
