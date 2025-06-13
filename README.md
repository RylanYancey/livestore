
# Livestore
A statically typed ultra lightweight alternative to Redis. 

Livestore is a Key/Value store intended for use in real-time websocket servers. All data is kept in-memory and unserialized, and is discarded when the application is stopped. 

## Features
- Insert/Remove entries (keys are UUID-V4)
- Powered by `DashMap` internally
- Prevent race conditions with asyncronous per-entry mutation locks
- Subscribe to mutations with async broadcast channels
- Dispatch mutations and non-mutating messages
- Set auto-expire timeouts

## Todos
- Rollback/Version tracking
- Serializing and Deserializing Stores

## Should you use Livestore?
Probably not. This is maintained by a 20-something year old college dropout in his free time, not by a conglomerate or corporation with millions of dollars. You will probably get better performance and way more features using Redis. That being said, if you hate the Redis API as much as I do, it just might be worth it. 

## Concepts
**Map-level Locks**\
Internally each store is a `DashMap`, a rust implementation of google's ABSL concurrent hashmap. This breaks down the map into a series of 8 (configurable) *shards*, each with its own lock. The priority when operating on entries in the map is to hold the Map-level lock for *as short a period as possible*, to reduce the probability of contention. We optimize this by cloning entry values out instead of returning a reference to entries like you might expect. 

**Entry-level Locks**\
A common operation is to read, compute changes, then assign a new value. But if it is possible for another thread to mutate the value while you are computing changes, then a race condition occurs. If not properly handled, this can lead to invalid states and/or an unexpected crash. We can't solve this problem by holding onto the Map-level lock because that creates a very high probability of contention, so each entry has a `tokio::sync::Mutex<()>` that prevents mutation while it is held. Note that this does not prevent the value from being read, only mutated or acquired. When a process calls `LiveStore::acquire().await`, the process may await until the lock is free. The returned `EntryGuard` guarantees that the value will not change until the guard is dropped. 

**Live Value**\
A type that implements `LiveValue` and exists in a `LiveStore`. 

**Mutation**\
An event that can be dispatched to mutate a value and inform any subscribed tasks of the change.

**Message**\
A non-mutating event that can be dispatched just like a Mutation and is sent to all subscribed tasks, but does not alter value state. 

## Usage Examples
**Implement LiveValue for your type**
```rs
use livestore::{LiveValue, Entry};

#[derive(Clone)] // LiveValues must be Clone
pub struct Foo {
    pub bar: u32,
    pub baz: u64,
}

#[derive(Clone)]
pub enum FooMessage {
    SendBar,
}

#[derive(Clone)]
pub enum FooMutation {
    SetBar(u32),
    SetBaz(u64),
}

pub enum FooError {
    BarWas42,
}

impl LiveValue for Foo {
    /// A message to subscribed async tasks
    /// that does not mutate the value.
    type Message = FooMessage;

    /// A message that executes a mutation on the contained value 
    /// before being broadcast to any subscribed async tasks.
    type Mutation = FooMutation;

    /// An Error that may occur while applying a mutation.
    type MutError = FooError;

    /// Apply a mutation to the entry, returning an error if the operation fails.
    /// 
    /// If the mutation is a success, an `Update::Mutate` is broadcast to any
    /// subscribed async tasks.
    fn mutate(entry: &Entry<Self>, mutation: &Self::Mutation) -> Result<Self, Self::MutError> {
        match mutation {
            FooMutation::SetBar(bar) => {
                if bar == 42 {
                    Err(FooError::BarWas42)
                } else {
                    Ok(Foo { 
                        bar, 
                        baz: entry.baz 
                    })
                }
            },
            FooMutation::SetBaz(baz) => {
                Ok(Foo { 
                    bar: entry.bar, 
                    baz 
                })
            }
        }
    }
}
```
**Initialize a LiveStore for your Type**
```rs
use std::time::Duration;
use livestore::{LiveStore, StoreOptions};

let foo_store = LiveStore::new(
    StoreOptions {
        /// The maximum number of mutations that
        /// can be stored in the value's broadcast queue.
        channel_cap: 4,

        /// The number of shards a store is split up into.
        /// This is passed to the inner `DashMap`. The value
        /// must be a power of 2 and non-zero. For low-access
        /// maps, you can get away with a lower shard amt, but
        /// high-access maps should have a higher shard amt.
        shard_amt: None,

        /// The frequency that the auto-expire check is ran.
        /// This requires iteration of _every_ value in the
        /// store, so it shouldn't be ran too often. 
        /// 
        /// If this value is `None`, the check will never run.
        expire_check_freq: Some(Duration::from_minutes(15)),

        /// The maximum amount of a time a value can live without
        /// being mutated before it will be auto-expired. 
        expire_timeout: Some(Duration::from_minutes(10)),
    }
);

let key = store.init(Foo { bar: 0, baz: 1 });

if let Some(entry) = store.fetch(&key) {
    println!("Bar: {}", entry.bar)
}
```
(Optional) **Configure an on-expire callback**
```rs
async fn on_foo_expire(expired: Vec<(Uuid, Entry<V>)>) {
    for (_, foo) in expired {
        println!("Baz '{}' expired.", foo.baz)
    }
}

foo_store.on_expire(on_foo_expire);
```
(Optional) **Add Stores to `Storages`**
```rs
use livestore::Storages;

// Storages is a convnience for erasing the type from LiveStores
let storages = Storages::new()
    .add_store(foo_store)
    .done();

// Access by downcasting from `Arc<dyn Any>`
let store = storages.as_store::<Foo>();
```
**Initialize Live Values in the Store**
```rs
let foo = Foo { bar: 1, baz: 0 };

// Generate a UUID and insert, returning the UUID.
let key = foo_store.init(foo.clone());

// Create a new entry without overwriting. 
if let Some(_) = foo_store.create(&key, foo.clone()) {
    println!("Did not create because the entry already exists.")
}

let new = Foo { bar: 2, baz: 9 };

// Assign a new value to the key, without creating a new value.
if let None = foo_store.assign(&key, new.clone()).await {
    println!("Did not Assign because the entry doesn't exist.");
}

// Insert combines the behavior of Assign and Create. It
// inserts if the key doesn't exist and overwrites if it does.
if let Some(old) = foo_store.insert(&key, new.clone()).await {
    println!("Bar '{}' was overwritten.", old.bar);
}

if let Some(entry) = foo_store.remove(&key).await {
    println!("Baz '{}' was removed.", entry.baz);
}
```
**Dispatch and Respond to Mutations**
```rs
let foo = Foo { bar: 3, baz: 1 };
let key = foo_store.init(foo.clone());

if let Some(watch) = foo_store.watch(&key) {
    watch.on_update(|update| {
        match update {
            Update::Overwrite { new, old, .. } => println!("Bar '{}' is now '{}'", old.bar, new.bar),
            Update::Remove { val, msg, .. } => println!("Baz '{}' was removed.", val.baz),
            Update::Mutate { new, old, .. } => println!("Bar '{}' is now '{}'", old.bar, new.bar),
            Update::Message { val, msg } => {
                match msg {
                    FooMessage::SendBar => println!("Bar: {}", val.bar),
                }
            }
        }
    });
}

// Dispatch a mutation to foo that will trigger an `Update::Mutate` on foo. 
foo_store.mutate(&key, FooMutation::SetBar(54));
```
**Acquire Entry Mutation Locks**
```rs

```