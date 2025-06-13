# livestore

A statically typed ultra lightweight alternative to Redis.

A LiveStore is a concurrent hashmap for use in real-time websocket servers.
Keys are always UUID-V4, and the values can be anything that implements `LiveValue`.

All data is kept in-memory in its native rust layout. This eliminates the overhead of
deserializing/serializing data on every access, but any data in the store will
be dropped when the store is dropped. We do not yet support any in-disk caching or saving/loading
of values.

### Features
- Insert/Remove/Fetch
- Powered by Google ABSL Hash Table (DashMap)
- Per-Entry mutation locking (prevents race conditions)
- Subscribe/Listen to value updates asyncronously (via Tokio broadcast)
- Auto-expire on timeout

### Todos
- Rollback/Version tracking
- Serializing and Deserializing Stores, save/load
- Lower per-entry memory usage

### Should you use LiveStore?
Probably not. This is maintained by a college dropout in his 20s, not a corporation or conglomerate with millions
of dollars. You will probably get better performance and features by just using Redis. But, if you hate dynamic
typing as much as I do, you might find some value in it.

### Concepts
**Map-level Locks (Sync)**\
Internally each store is a `DashMap`, which is a HashMap broken up into "shards", or parts. Each shard has its own
RwLock that needs to be acquired when reading or writing to its entries. To minimize the probability of contention,
LiveStore tries to hold on to this lock for as short a period as possible. Thats' why none of the functions return a
reference to entries, its always a clone.

**Entry-level Locks (Async)**\
A common operation is to read, compute changes, then assign a new value. But if another process mutates the value
while another is computing changes, a race conditions occurs. This can lead to invalid states and unexpected crashes.
To solve this, we could hold onto the map lock for the duration we are computing changes, but that would make the probability
of contention too high, especially if the lock is held through an await. So instead there is a per-entry Mutex that
can be acquired to prevent the state from being invalidated for the duration it is held. When a function with `.acquire`
is called, an [`EntryGuard`] is returned, preventing any mutations or acquisitions until it is dropped.

**Live Value**\
A type that implements `LiveValue` and exists in a `LiveStore`.

**Mutation**\
An event that can be dispatched to mutate a value and inform any subscribed tasks of the change.

**Message**\
An event that can be dispatched just like a mutation and is sent to subscribed tasks, but does not alter value state.

### Usage
The first step is to implement `LiveValue` for your type. The associated types (Message, Mutation, MutError) are optional
and can be set to `()` if you don't use them.

Then, create a `LiveStore` with the builder via `LiveStore::new()` or use the provided defaults with `LiveStore::default()`.

If you want a type-erased container, this crate also provides the `Storages` type, a convenience for erasing the
value type and accessing with `Any::downcast()`.
```rust
use std::time::Duration;
use livestore::{LiveStore, LiveValue, Entry, Update, StoreOptions, ExpireMode, Uuid};

/// **Your Value Type**
#[derive(Clone)]
struct Foo {
    bar: u32,
    baz: u64
}

#[derive(Clone)]
enum FooMessage {
    PrintBar
}

#[derive(Clone)]
enum FooMutation {
    SetBar(u32),
    SetBaz(u64)
}

#[derive(Clone)]
enum FooError {
    BarWas42
}

// **Implement LiveValue for your Type**
impl LiveValue for Foo {
    type Message = FooMessage;
    type Mutation = FooMutation;
    type MutError = FooError;

    fn mutate(entry: &Entry<Self>, mutation: &Self::Mutation) -> Result<Self, Self::MutError> {
        match mutation {
            FooMutation::SetBar(bar) => {
                if *bar == 42 {
                    Err(FooError::BarWas42)
                } else {
                    Ok(Foo {
                        bar: *bar,
                        baz: entry.baz
                    })
                }
            },
            FooMutation::SetBaz(baz) => {
                Ok(Foo {
                    bar: entry.bar,
                    baz: *baz
                })
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let store = LiveStore::<Foo>::new()
        // Configure the store to auto-remove entries
        // that haven't been mutated in 10 minutes, checked every 15 minutes.
        .with_options(
            StoreOptions {
                expire_check_freq: Some(Duration::from_secs(900)),
                expire_mode: ExpireMode::Timeout(Duration::from_secs(600)),
                ..Default::default()
            }
        )
        // **Set an expiration callback**
        .on_expire(move |expired: Vec<(Uuid, Entry<Foo>)>| {
            async move {
                for (_, item) in expired {
                    println!("Baz '{}' expired.", item.baz);
                }
            }
        })
        .done();

    // Initialize values
    let k1 = store.init(Foo { bar: 0, baz: 1 });
    let k2 = store.init(Foo { bar: 1, baz: 0 });

    // Begin watching for changes
    let mut sub = store.watch(&k1).unwrap();
    tokio::spawn(async move {
        while let Some(update) = sub.next().await {
            match update {
                Update::Mutate { mutn, .. } => {
                    match mutn {
                        FooMutation::SetBar(bar) => println!("Set bar to '{bar}'."),
                        FooMutation::SetBaz(baz) => println!("Set baz to '{baz}'."),
                    }
                },
                Update::Message { msg, val } => {
                    match msg {
                        FooMessage::PrintBar => println!("Bar: {}", val.bar),
                    }
                },
                _ => {}
            }
        }
    });

    // Dispatch mutations and broadcast messages.
    store.mutate(&k1, FooMutation::SetBar(6)).await;
    store.mutate(&k1, FooMutation::SetBaz(9)).await;
    store.broadcast(&k1, FooMessage::PrintBar);

    // Acquire mutation locks.
    let mut entry2 = store.acquire(&k2).await.unwrap();
    entry2.mutate(FooMutation::SetBar(7));
    entry2.mutate(FooMutation::SetBaz(10));
    entry2.broadcast(FooMessage::PrintBar);

    // Remove entries from the store.
    entry2.delete();
    store.remove(&k1).await.unwrap();
}
```
