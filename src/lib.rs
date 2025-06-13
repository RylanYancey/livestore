
//! A statically typed ultra lightweight alternative to Redis.
//! 
//! A LiveStore is a concurrent hashmap for use in real-time websocket servers. 
//! Keys are always UUID-V4, and the values can be anything that implements `LiveValue`. 
//! 
//! All data is kept in-memory in its native rust layout. This eliminates the overhead of 
//! deserializing/serializing data on every access, but any data in the store will
//! be dropped when the store is dropped. We do not yet support any in-disk caching or saving/loading 
//! of values.
//! 
//! ## Features
//! - Insert/Remove/Fetch
//! - Powered by Google ABSL Hash Table (DashMap)
//! - Per-Entry mutation locking (prevents race conditions)
//! - Subscribe/Listen to value updates asyncronously (via Tokio broadcast)
//! - Auto-expire on timeout
//! 
//! ## Todos
//! - Rollback/Version tracking
//! - Serializing and Deserializing Stores, save/load
//! - Lower per-entry memory usage
//! 
//! ## Should you use LiveStore?
//! Probably not. This is maintained by a college dropout in his 20s, not a corporation or conglomerate with millions 
//! of dollars. You will probably get better performance and features by just using Redis. But, if you hate dynamic
//! typing as much as I do, you might find some value in it. 
//! 
//! ## Concepts
//! **Map-level Locks (Sync)**\
//! Internally each store is a `DashMap`, which is a HashMap broken up into "shards", or parts. Each shard has its own
//! RwLock that needs to be acquired when reading or writing to its entries. To minimize the probability of contention,
//! LiveStore tries to hold on to this lock for as short a period as possible. Thats' why none of the functions return a
//! reference to entries, its always a clone. 
//! 
//! **Entry-level Locks (Async)**\
//! A common operation is to read, compute changes, then assign a new value. But if another process mutates the value
//! while another is computing changes, a race conditions occurs. This can lead to invalid states and unexpected crashes.
//! To solve this, we could hold onto the map lock for the duration we are computing changes, but that would make the probability
//! of contention too high, especially if the lock is held through an await. So instead there is a per-entry Mutex that
//! can be acquired to prevent the state from being invalidated for the duration it is held. When a function with `.acquire`
//! is called, an [`EntryGuard`] is returned, preventing any mutations or acquisitions until it is dropped. 
//! 
//! **Live Value**\
//! A type that implements `LiveValue` and exists in a `LiveStore`.
//! 
//! **Mutation**\
//! An event that can be dispatched to mutate a value and inform any subscribed tasks of the change.
//! 
//! **Message**\
//! An event that can be dispatched just like a mutation and is sent to subscribed tasks, but does not alter value state.
//! 
//! ## Usage
//! The first step is to implement `LiveValue` for your type. The associated types (Message, Mutation, MutError) are optional
//! and can be set to `()` if you don't use them. 
//! 
//! Then, create a `LiveStore` with the builder via `LiveStore::new()` or use the provided defaults with `LiveStore::default()`. 
//! 
//! If you want a type-erased container, this crate also provides the `Storages` type, a convenience for erasing the
//! value type and accessing with `Any::downcast()`.
//! ```
//! use std::time::Duration;
//! use livestore::{LiveStore, LiveValue, Entry, Update, StoreOptions, ExpireMode, Uuid};
//! 
//! /// **Your Value Type**
//! #[derive(Clone)]
//! struct Foo {
//!     bar: u32,
//!     baz: u64
//! }
//! 
//! #[derive(Clone)]
//! enum FooMessage {
//!     PrintBar
//! }
//! 
//! #[derive(Clone)]
//! enum FooMutation {
//!     SetBar(u32),
//!     SetBaz(u64)   
//! }
//! 
//! #[derive(Clone)]
//! enum FooError {
//!     BarWas42
//! }
//! 
//! // **Implement LiveValue for your Type**
//! impl LiveValue for Foo {
//!     type Message = FooMessage;
//!     type Mutation = FooMutation;
//!     type MutError = FooError;
//! 
//!     fn mutate(entry: &Entry<Self>, mutation: &Self::Mutation) -> Result<Self, Self::MutError> {
//!         match mutation {
//!             FooMutation::SetBar(bar) => {
//!                 if *bar == 42 {
//!                     Err(FooError::BarWas42)
//!                 } else {
//!                     Ok(Foo { 
//!                         bar: *bar, 
//!                         baz: entry.baz 
//!                     })
//!                 }
//!             },
//!             FooMutation::SetBaz(baz) => {
//!                 Ok(Foo { 
//!                     bar: entry.bar, 
//!                     baz: *baz
//!                 })
//!             }
//!         }
//!     }
//! }
//! 
//! #[tokio::main]
//! async fn main() {
//!     let store = LiveStore::<Foo>::new()
//!         // Configure the store to auto-remove entries
//!         // that haven't been mutated in 10 minutes, checked every 15 minutes.
//!         .with_options(
//!             StoreOptions {
//!                 expire_check_freq: Some(Duration::from_secs(900)),
//!                 expire_mode: ExpireMode::Timeout(Duration::from_secs(600)),
//!                 ..Default::default()
//!             }
//!         )
//!         // **Set an expiration callback**
//!         .on_expire(move |expired: Vec<(Uuid, Entry<Foo>)>| {
//!             async move {
//!                 for (_, item) in expired {
//!                     println!("Baz '{}' expired.", item.baz);
//!                 }
//!             }
//!         })
//!         .done();
//!     
//!     // Initialize values
//!     let k1 = store.init(Foo { bar: 0, baz: 1 });
//!     let k2 = store.init(Foo { bar: 1, baz: 0 });
//!     
//!     // Begin watching for changes
//!     let mut sub = store.watch(&k1).unwrap();
//!     tokio::spawn(async move {
//!         while let Some(update) = sub.next().await {
//!             match update {
//!                 Update::Mutate { mutn, .. } => {
//!                     match mutn {
//!                         FooMutation::SetBar(bar) => println!("Set bar to '{bar}'."),
//!                         FooMutation::SetBaz(baz) => println!("Set baz to '{baz}'."),
//!                     }
//!                 },
//!                 Update::Message { msg, val } => {
//!                     match msg {
//!                         FooMessage::PrintBar => println!("Bar: {}", val.bar),
//!                     }
//!                 },
//!                 _ => {}
//!             }
//!         }
//!     });
//! 
//!     // Dispatch mutations and broadcast messages.
//!     store.mutate(&k1, FooMutation::SetBar(6)).await;
//!     store.mutate(&k1, FooMutation::SetBaz(9)).await;
//!     store.broadcast(&k1, FooMessage::PrintBar);
//! 
//!     // Acquire mutation locks.
//!     let mut entry2 = store.acquire(&k2).await.unwrap();
//!     entry2.mutate(FooMutation::SetBar(7));
//!     entry2.mutate(FooMutation::SetBaz(10));
//!     entry2.broadcast(FooMessage::PrintBar);
//! 
//!     // Remove entries from the store.
//!     entry2.delete();
//!     store.remove(&k1).await.unwrap();
//! }
//! ```

use std::{any::{type_name, Any, TypeId}, ops::Deref, pin::Pin, sync::Arc, time::{Duration, Instant}};

use dashmap::{mapref::one::{Ref, RefMut}, DashMap};
use tokio::sync::{broadcast::{error::RecvError, Receiver, Sender}, Mutex, OwnedMutexGuard};

/// Uuid is re-exported for continuity.
pub use uuid::Uuid;

/// A Convenience for passing around type-erased LiveStores. 
pub struct Storages {
    typeids: boomphf::Mphf<TypeId>,
    stores: Vec<Arc<dyn StoreType>>,
}

pub trait StoreType: Any + Send + Sync { }

pub struct StoragesBuilder {
    stores: Vec<Arc<dyn StoreType>>,
}

pub trait LiveValue: Clone + Sized + Send + Sync + Any {
    /// A message to subscribed async tasks
    /// that does not mutate the value.
    /// 
    /// Can be set to `()` if unused.
    type Message: Clone + Send + Sync;

    /// A message that executes a mutation on the contained value 
    /// before being broadcast to any subscribed async tasks.
    /// 
    /// Can be set to `()` if unused.
    type Mutation: Clone + Send + Sync;

    /// An Error that may occur while applying a mutation.
    /// 
    /// Can be set to `()` if unused.
    type MutError;

    /// Apply a mutation to the entry, returning an error if the operation fails.
    /// 
    /// If the mutation is a success, an `Update::Mutate` is broadcast to any
    /// subscribed async tasks.
    /// 
    /// If Mutation is unused, this can safely return `Ok(())`.
    fn mutate(entry: &Entry<Self>, mutation: &Self::Mutation) -> Result<Self, Self::MutError>;
}

/// Storage for entries with type V and broadcast 
/// channels for pub/sub capability.
pub struct LiveStore<V: LiveValue> {
    map: DashMap<Uuid, RawEntry<V>>,
    options: StoreOptions,
    on_expire: Option<AsyncCallback<Vec<(Uuid, Entry<V>)>>>,
}

#[derive(Copy, Clone)]
pub struct StoreOptions {
    /// The maximum number of mutations that
    /// can be stored in the value's broadcast queue.
    pub channel_cap: usize,

    /// The number of shards a store is split up into.
    /// This is passed to the inner `DashMap`. The value
    /// must be a power of 2 and non-zero. For low-access
    /// maps, you can get away with a lower shard amt, but
    /// high-access maps should have a higher shard amt.
    pub shard_amt: Option<usize>,

    /// The frequency that the auto-expire check is ran.
    /// This requires iteration of _every_ value in the
    /// store, so it shouldn't be ran too often. 
    /// 
    /// If this value is `None`, the check will never run.
    pub expire_check_freq: Option<Duration>,

    /// The conditions of expiration.
    pub expire_mode: ExpireMode,
}

pub struct LiveStoreBuilder<V: LiveValue> {
    options: StoreOptions,
    on_expire: Option<AsyncCallback<Vec<(Uuid, Entry<V>)>>>,
}

#[derive(Copy, Clone)]
pub enum ExpireMode {
    Timeout(Duration),
}

/// Raw entry data, including the transmitter
/// for broadcasting value changes and a lock
/// for synchronizing access.
/// 
/// Mutating the value is done by replacing the
/// `value` field with a new Arc, rather than
/// assigning to the value directly. 
struct RawEntry<V: LiveValue> {
    value: Arc<V>,
    /// Timestamp of last mutation or overwrite.
    last_change: Instant,

    /// Timestamp of entry creation. 
    create_time: Instant,

    /// Channel for sending state updates to 
    /// subscribed async tokio tasks.
    broadcast: Sender<Update<V>>,

    /// A lock for synchronizing access. This is not included over `value` 
    /// because it does not _truly_ enforce immutability while held. It is just
    /// there to prevent race conditions or state invalidations while mutating
    /// the data. The user may want to get the value while it is acquired,
    /// for example. We can do this because the user has to have a lock over the
    /// _Map_ itself to actually mutate the value's contents.
    lock: Arc<Mutex<()>>,
}

/// Value and Metadata of an Entry in the LiveStore.
/// 
/// This structure does not hold a lock to the
/// entry in the store, see [`EntryGuard`] instead.
#[derive(Clone)]
pub struct Entry<V: LiveValue> {
    pub value: Arc<V>,
    pub last_change: Instant,
    pub create_time: Instant,
}

/// Value and Metadata of an Entry in the LiveStore.
/// 
/// This structure holds a lock its entry.
/// Attempts to mutate the entry that other threads will 
/// be blocked until this guard is dropped or `.release()` is called.
pub struct EntryGuard<V: LiveValue> {
    pub value: Arc<V>,
    pub last_change: Instant,
    pub create_time: Instant,
    uuid: Uuid,
    store: Arc<LiveStore<V>>,
    _guard: OwnedMutexGuard<()>,
}

/// A change to a value in the LiveStore that is
/// sent to subscribed async tasks. 
#[derive(Clone)]
pub enum Update<V: LiveValue> {
    Remove {
        msg: Option<V::Message>,
        val: Arc<V>,
        key: Uuid,
    },
    Overwrite {
        msg: Option<V::Message>,
        new: Entry<V>,
        old: Entry<V>,
    },
    Mutate {
        mutn: V::Mutation,
        new: Entry<V>,
        old: Entry<V>,
    },
    Message {
        msg: V::Message,
        val: Entry<V>,
    }
}

/// A wrapper over a `tokio::sync::broadcast::Receiver` for
/// listening to mutations and messages for a LiveValue. 
/// 
/// Use `.next().await` to start listening for updates.
pub struct Watch<V: LiveValue> {
    pub rx: Receiver<Update<V>>,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Control {
    Break, Continue
}

impl Storages {
    pub fn new() -> StoragesBuilder {
        StoragesBuilder {
            stores: Vec::new(),
        }
    }

    /// Get a reference to the ValueStore for a specific type.
    pub fn as_store<V: LiveValue>(&self) -> Arc<LiveStore<V>> {
        let id = TypeId::of::<V>();
        let idx = self.typeids.hash(&id) as usize;
        match self.stores.get(idx) {
            None => panic_uninit_value_type::<V>(),
            Some(store) => {
                match Arc::downcast::<LiveStore<V>>(store.clone()) {
                    Err(_) => panic_uninit_value_type::<V>(),
                    Ok(store) => store
                }
            }
        }
    }
}

impl StoragesBuilder {
    pub fn add_store<V: LiveValue>(mut self, store: Arc<LiveStore<V>>) -> Self {
        self.stores.push(store);
        self
    }

    pub fn done(self) -> Storages {
        let typeids = self.stores.iter().map(|store| store.type_id()).collect::<Vec<_>>();
        Storages {
            typeids: boomphf::Mphf::new(2.2, &typeids),
            stores: self.stores
        }
    }
}

impl<V: LiveValue> LiveStore<V> {
    /// Initialize the store with the provided options.
    pub fn new() -> LiveStoreBuilder<V> {
        LiveStoreBuilder {
            options: StoreOptions::default(),
            on_expire: None,
        }
    }

    /// Returns a [`Watch`] struct that wraps a `tokio::sync::broadcast::Receiver`
    /// that will receive updates when the value at this key changes.
    /// 
    /// [`Watch`] provides an `on_update` function that can be used to start 
    /// receiving updates, or you can handle the receiver yourself with the
    /// public `rx` field.
    pub fn watch(&self, key: &Uuid) -> Option<Watch<V>> {
        self.map.get(key).map(|raw| raw.watch())
    }

    /// Acquire the entry lock and fetch the value. 
    /// 
    /// Until the returned Guard is dropped, any attempts to acquire the lock
    /// for this entry will have to await. This is useful if you want to read
    /// then mutate an entry without causing a race condition.
    /// 
    /// # Deadlock Behavior
    /// 
    /// If a thread attempts to acquire a guard from a store while 1 or more guards
    /// already exist on the thread, a deadlock is certain to occur eventually. It is
    /// best practice for each thread to only hold one [`EntryGuard`] at a time. Guards can
    /// be released by calling their drop destructor.
    pub async fn acquire(self: &Arc<Self>, key: &Uuid) -> Option<EntryGuard<V>> {
        self.acquire_raw(key).await.map(|(raw, guard)| EntryGuard::new(*key, &self, &raw, guard))
    }

    /// Fetch the value without checking or acquiring the entry lock.
    /// 
    /// Unlike `.acquire()`, this function will just take the value of
    /// the entry as it is, even if its lock is acquired. This is safe 
    /// because an entry can only be mutated if the lock on the _Map_
    /// is acquired, but it does mean the returned value may be invalidated
    /// the moment this function drops its map lock. 
    pub fn fetch(&self, key: &Uuid) -> Option<Entry<V>> {
        self.map.get(key).map(|raw| Entry::from_raw(&raw))
    }

    /// Fetch the value without checking or acquiring the entry lock, and subscribe
    /// to updates on the value.
    /// 
    /// Caveats on `LiveStore::fetch` still apply here.
    pub fn fetch_and_watch(&self, key: &Uuid) -> Option<(Entry<V>, Watch<V>)> {
        self.map.get(key).map(|raw| (Entry::from_raw(&raw), raw.watch()))
    }

    /// Initialize a new entry with the value, returning the Uuid that was
    /// generated and assigned to it. 
    pub fn init(&self, val: V) -> Uuid {
        let uuid = Uuid::new_v4();
        // `init` cannot overwrite because the Uuid is unique.
        self.map.insert(uuid, RawEntry::new(&self.options, Arc::new(val)));
        uuid
    }

    /// Initialize a new entry with the value, returning the Uuid that was
    /// assigned to it and a subscription to value updates.
    pub fn init_and_watch(&self, val: V) -> (Uuid, Watch<V>) {
        let uuid = Uuid::new_v4();
        let entry = RawEntry::new(&self.options, Arc::new(val));
        let watch = entry.watch();
        self.map.insert(uuid, entry);
        (uuid, watch)
    }

    /// Initialize a new key with the value and acquire its lock.
    /// Access the generated UUID through `EntryGuard::key()`.
    /// 
    /// This function WILL acquire the entry lock. All deadlock conditions 
    /// on `LiveStore::assign` apply here.
    pub fn init_and_acquire(self: &Arc<Self>, val: V) -> EntryGuard<V> {
        let uuid = Uuid::new_v4();
        let entry = RawEntry::new(&self.options, Arc::new(val));
        let guard = EntryGuard::new(uuid, &self, &entry, entry.lock.clone().blocking_lock_owned());
        self.map.insert(uuid, entry);
        guard
    }

    /// Insert the value at the key without overwriting an existing value.
    /// Returns `Some(existing_val)` if the key already exists in the store.
    pub fn create(&self, key: &Uuid, val: V) -> Option<Entry<V>> {
        let mut existing: Option<Entry<V>> = None;
        self.map.entry(*key)
            .and_modify(|raw| existing = Some(Entry::from_raw(raw)))
            .or_insert_with(|| RawEntry::new(&self.options, Arc::new(val)));
        existing
    }

    /// Insert the value at the key without overwriting an existing value. 
    /// Returns `Some(existing_val)` if the key already exists in the store.
    /// Also returns a [`Watch`] struct that can be used to respond to value updates.
    pub fn create_and_watch(&self, key: &Uuid, val: V) -> (Option<Entry<V>>, Watch<V>) {
        let mut existing: Option<Entry<V>> = None;
        let raw = self.map.entry(*key)
            .and_modify(|raw| existing = Some(Entry::from_raw(raw)))
            .or_insert_with(|| RawEntry::new(&self.options, Arc::new(val)));
        (existing, raw.watch())
    }

    /// Insert the value at the key without overwriting an existing value.
    /// Returns an [`EntryGuard`] that guarantees the value isn't mutated
    /// while it is held.
    /// 
    /// This function WILL acquire the entry lock. All deadlock conditions 
    /// on `LiveStore::assign` apply here.
    pub async fn create_and_acquire(self: &Arc<Self>, key: &Uuid, val: V) -> EntryGuard<V> {
        self.map.entry(*key)
            .or_insert_with(|| RawEntry::new(&self.options, Arc::new(val)));
        self.acquire(key).await.unwrap()
    }

    /// Insert the value at the key, overwriting any existing value.
    /// If an overwrite occurs, an update will be dispatched and the
    /// previous value will be returned.
    /// 
    /// This function WILL acquire the entry lock. All deadlock conditions 
    /// on `LiveStore::assign` apply here.
    pub async fn insert(&self, key: &Uuid, val: V) -> Option<Entry<V>> {
        self.insert_then(key, val, |raw, guard| guard.map(|_| Entry::from_raw(&raw))).await?
    }

    /// Insert the value at the key, overwriting any existing value.
    /// If an overwrite occurs, an update will be dispatched and the
    /// previous value will be returned.
    /// 
    /// This function WILL acquire the entry lock. All deadlock conditions 
    /// on `LiveStore::assign` apply here.
    pub async fn insert_and_watch(&self, key: &Uuid, val: V) -> Option<Entry<V>> {
        self.insert_then(key, val, |raw, guard| guard.map(|_| Entry::from_raw(&raw))).await?
    }

    /// Overwrite the value at the key. This function will NOT insert if
    /// the key does not exist. If an overwrite occurs, an update will be
    /// dispatched and the previous value will be returned.
    /// 
    /// This function WILL acquire the entry lock. All deadlock conditions 
    /// on `LiveStore::assign` apply here.
    pub async fn assign(self: &Arc<Self>, key: &Uuid, val: V) -> Option<Entry<V>> {
        self.acquire_raw_mut(key).await.map(|(mut raw, _)| {
            let old = Entry::from_raw(&raw);
            raw.change(Arc::new(val), None, None);
            old
        })
    }

    /// Overwrite the value at the key. This function will NOT insert if
    /// the key does not exist. If an overwrite occurs, an update will be
    /// dispatched and the previous value will be returned.
    /// 
    /// Also returns a subscription to the value.
    ///
    /// This function WILL acquire the entry lock. All deadlock conditions 
    /// on `LiveStore::assign` apply here. 
    pub async fn assign_and_watch(self: &Arc<Self>, key: &Uuid, val: V) -> Option<(Entry<V>, Watch<V>)> {
        self.acquire_raw_mut(key).await.map(|(mut raw, _)| {
            let old = Entry::from_raw(&raw);
            raw.change(Arc::new(val), None, None);
            (old, raw.watch())
        })
    }

    /// Overwrite the value at the key. This function will NOT insert if
    /// the key does not exist. If an overwrite occurs, an update will be
    /// dispatched and the previous value will be returned.
    /// 
    /// Also returns a guard over the new entry value.
    /// 
    /// This function WILL acquire the entry lock. All deadlock conditions 
    /// on `LiveStore::assign` apply here.
    pub async fn assign_and_acquire(self: &Arc<Self>, key: &Uuid, val: V) -> Option<(Entry<V>, EntryGuard<V>)> {
        self.acquire_raw_mut(key).await.map(|(mut raw, guard)| {
            let old = Entry::from_raw(&raw);
            raw.change(Arc::new(val), None, None);
            (old, EntryGuard::new(*key, &self, &raw, guard))
        })
    }

    async fn acquire_raw(&self, key: &Uuid) -> Option<(Ref<'_, Uuid, RawEntry<V>>, OwnedMutexGuard<()>)> {
        // Attempt to acquire, returning the Arc<Mutex> lock it fails.
        let lock = match self.map.get(key) {
            None => return None,
            Some(raw) => {
                match raw.lock.clone().try_lock_owned() {
                    Ok(guard) => return Some((raw, guard)),
                    _ => raw.lock.clone()
                }
            }
        };

        // Poll the lock until ready.
        // This is done outside of the `match` statement
        // above to ensure the lock on the Dashmap is dropped
        // while we are awaiting.
        let guard = lock.lock_owned().await;

        // try to get the value again now that we have the lock.
        self.map.get(key).map(|raw| (raw, guard))
    }

    async fn acquire_raw_mut(&self, key: &Uuid) -> Option<(RefMut<'_, Uuid, RawEntry<V>>, OwnedMutexGuard<()>)> {
        // Attempt to acquire, returning the Arc<Mutex> lock it fails.
        let lock = match self.map.get_mut(key) {
            None => return None,
            Some(raw) => {
                match raw.lock.clone().try_lock_owned() {
                    Ok(guard) => return Some((raw, guard)),
                    _ => raw.lock.clone()
                }
            }
        };

        // Poll the lock until ready.
        // This is done outside of the `match` statement
        // above to ensure the lock on the Dashmap is dropped
        // while we are awaiting.
        let guard = lock.lock_owned().await;

        // try to get the value again now that we have the lock.
        self.map.get_mut(key).map(|raw| (raw, guard))
    }

    async fn insert_then<Out>(
        &self, key: &Uuid, val: V,
        then: impl FnOnce(&mut RawEntry<V>, Option<OwnedMutexGuard<()>>) -> Out
    ) -> Option<Out> {
        let val = Arc::new(val);
        let mut old: Option<Entry<V>> = None;
        let lock: Arc<Mutex<()>>;
        {
            let mut raw = self.map.entry(*key)
                .and_modify(|raw| old = Some(Entry::from_raw(&raw)))
                .or_insert_with(|| RawEntry::new(&self.options, val.clone()));

            if old.is_none() {
                return Some((then)(&mut raw, None));
            } 

            if let Ok(guard) = raw.lock.clone().try_lock_owned() {
                raw.change(val, None, None);
                return Some((then)(&mut raw, Some(guard)));
            } else {
                lock = raw.lock.clone()
            }
        }

        let guard = lock.lock_owned().await;
        if let Some(mut raw) = self.map.get_mut(key) {
            raw.change(val, None, None);
            return Some((then)(&mut raw, Some(guard)))
        }

        None
    }

    /// Remove an entry from the Store, returning the value.
    pub async fn remove(self: &Arc<Self>, key: &Uuid) -> Option<Entry<V>> {
        self.acquire(key).await.map(|guard| {
            let ret = guard.as_entry();
            guard.delete();
            ret
        })
    }

    /// Dispatch a mutation for the entry at the key. 
    /// 
    /// If the key does not exist, None is returned. 
    /// If the key exists and mutation was successful, the new entry is returned and the update is broadcast.
    /// If the key exists and mutation failed, the error is returned and no updates are broadcast. 
    pub async fn mutate(self: &Arc<Self>, key: &Uuid, mutn: V::Mutation) -> Option<Result<Entry<V>, V::MutError>> {
        self.acquire_raw_mut(key).await.map(|(mut raw, _guard)| {
            let res = V::mutate(&Entry::from_raw(&raw), &mutn);
            res.map(|new| {
                raw.change(Arc::new(new), None, Some(mutn));
                Entry::from_raw(&raw)
            })
        })
    }

    /// Dispatch a message to subscribed tasks of the entry at the key.
    /// 
    /// If the key does not exist, the message is returned. 
    pub fn broadcast(&self, key: &Uuid, msg: V::Message) -> Option<V::Message> {
        if let Some(raw) = self.map.get(key) {
            let _ = raw.broadcast.send(Update::Message { msg, val: Entry::from_raw(&raw) });
            None
        } else {
            Some(msg)
        }
    }

    pub fn default() -> Arc<Self> {
        Arc::new(
            Self {
                map: DashMap::new(),
                options: StoreOptions::default(),
                on_expire: None,
            }
        )
    }
    /// Remove any entries where the last change is before the minimum allowed time.
    fn expire_by_timeout(&self, min: Instant) {
        self.map.retain(|_, raw| raw.last_change < min && raw.lock.try_lock().is_ok())
    }

    /// Collect expired entries where the last change is before the minimum allowed time.
    fn collect_expired_by_timeout(&self, min: Instant) -> Vec<(Uuid, Entry<V>)> {
        let mut expired = Vec::new();
        self.map.retain(|uuid, raw| {
            if raw.last_change < min && raw.lock.try_lock().is_ok() {
                expired.push((*uuid, Entry::from_raw(&raw)));
                false   
            } else {
                true
            }
        });
        expired
    }
}


impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            channel_cap: 4,
            shard_amt: None,
            expire_check_freq: None,
            expire_mode: ExpireMode::Timeout(Duration::from_secs(60*60))
        }
    }
}

impl<V: LiveValue> LiveStoreBuilder<V> {
    pub fn with_options(mut self, options: StoreOptions) -> Self {
        self.options = options;
        self
    }

    /// Set the function called whenever the expire callback is ran.
    /// 
    /// If you want the callback to capture state, you need to wrap an `async move {}` block 
    /// in a regular function. Basically, you need to pass in a regular function that returns
    /// a future, an `async ||` closure won't work. It's annoying but its what
    /// the async overlords want. It will look like this:
    /// 
    /// `store.on_expire(move |expired| { /* capture data here */ async move { /* do something  with captured data*/ } });`
    pub fn on_expire(mut self, fut: impl Into<AsyncCallback<Vec<(Uuid, Entry<V>)>>>) -> Self {
        self.on_expire = Some(fut.into());
        self
    }

    /// Build the LiveStore from this builder.
    /// 
    /// This will also start the on_expire interval if configured.
    pub fn done(self) -> Arc<LiveStore<V>> {
        // Construct the map
        let store = Arc::new(
            LiveStore {
                map: self.options.shard_amt.map(|amt| DashMap::with_shard_amount(amt)).unwrap_or_default(),
                options: self.options,
                on_expire: self.on_expire,
            }
        );

        // Dispatch auto-expire task.
        // This tasks downgrades the Store's Arc to a `Weak`, so you'll
        // be able to drop the store and this task will end. 
        //
        // This task will respond to `ctrl_c` as well.
        if let Some(check_dur) = store.options.expire_check_freq {
            let store = Arc::downgrade(&store);

            tokio::spawn(async move {
                // timer that resets when it completes.
                let mut interval = tokio::time::interval(check_dur);
                // ignore first tick (completes instantly)
                interval.tick().await;
    
                'outer: loop {
                    // Select between ctrl_c and the interval, whichever completes first.
                    tokio::select! {
                        _ = interval.tick() => {
                            let Some(store) = store.upgrade() else { break 'outer; };
                            if let Some(on_expire) = &store.on_expire {
                                match store.options.expire_mode {
                                    ExpireMode::Timeout(dur) => {
                                        // Timeout + OnExpire
                                        let min = Instant::now() - dur;
                                        let expired = store.collect_expired_by_timeout(min);
                                        if !expired.is_empty() {
                                            on_expire.call(expired).await;
                                        }
                                    }
                                }
                            } else {
                                match store.options.expire_mode {
                                    ExpireMode::Timeout(dur) => {
                                        // Timeout + NO OnExpire cb
                                        let min = Instant::now() - dur;
                                        store.expire_by_timeout(min);
                                    }
                                }
                            }
                        },
                        _ = tokio::signal::ctrl_c() => {
                            break 'outer;
                        }
                    }
                }
            });
        }

        store
    }
}

impl<V: LiveValue> RawEntry<V> {
    fn new(opt: &StoreOptions, val: Arc<V>) -> Self {
        Self {
            value: val,
            last_change: Instant::now(),
            create_time: Instant::now(),
            broadcast: Sender::new(opt.channel_cap),
            lock: Arc::new(Mutex::new(()))
        }
    }

    #[inline]
    fn change(&mut self, val: Arc<V>, msg: Option<V::Message>, mutn: Option<V::Mutation>) {
        let old = Entry::from_raw(&self);
        self.last_change = Instant::now();
        self.value = val;
        let new = Entry::from_raw(&self);
        let update = if let Some(mutn) = mutn {
            Update::Mutate { mutn, new, old }
        } else {
            Update::Overwrite { msg, new, old }
        };
        let _ = self.broadcast.send(update);
    }

    #[inline]
    fn watch(&self) -> Watch<V> {
        Watch { rx: self.broadcast.subscribe() }
    }
}

impl<V: LiveValue> Entry<V> {
    #[inline]
    fn from_raw(raw: &RawEntry<V>) -> Self {
        Self {
            value: raw.value.clone(),
            last_change: raw.last_change,
            create_time: raw.create_time,
        }
    }
}

impl<V: LiveValue> Deref for Entry<V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<V: LiveValue> EntryGuard<V> {
    #[inline]
    fn new(uuid: Uuid, store: &Arc<LiveStore<V>>, raw: &RawEntry<V>, guard: OwnedMutexGuard<()>) -> Self {
        Self {
            uuid,
            value: raw.value.clone(),
            last_change: raw.last_change,
            create_time: raw.create_time,
            store: store.clone(),
            _guard: guard
        }
    }

    pub fn key(&self) -> &Uuid {
        &self.uuid
    }

    pub fn value(&self) -> &V {
        &self.value
    }

    pub fn as_entry(&self) -> Entry<V> {
        Entry {
            value: self.value.clone(),
            last_change: self.last_change,
            create_time: self.create_time,
        }
    }

    /// Subscribe to changes of this value.
    /// 
    /// The returned [`Watch`] struct can be used to
    /// poll for and respond to updates with async tasks
    /// by using the `on_update` function.
    pub fn watch(&self) -> Option<Watch<V>> {
        self.store.map.get(&self.uuid).map(|raw| 
            raw.watch()
        )
    }

    /// Broadcast a message to all subscribed async tasks.
    pub fn broadcast(&self, msg: V::Message) {
        if let Some(raw) = self.store.map.get(&self.uuid) {
            let _ = raw.broadcast.send(
                Update::Message {
                    val: Entry::from_raw(&raw),
                    msg
                }
            );
        }
    }

    /// Assign a new value to the key.
    /// 
    /// This will dispatch an `Update::Overwrite` to all
    /// subscribed async tasks.
    pub fn set(&mut self, val: V) {
        if let Some(mut raw) = self.store.map.get_mut(&self.uuid) {
            self.value = Arc::new(val);
            raw.change(self.value.clone(), None, None);
            self.last_change = raw.last_change;
        }
    }

    /// Assign a new value to the key with an attached message.
    pub fn set_with_msg(&mut self, val: V, msg: V::Message) {
        if let Some(mut raw) = self.store.map.get_mut(&self.uuid) {
            self.value = Arc::new(val);
            raw.change(self.value.clone(), Some(msg), None);
            self.last_change = raw.last_change;
        }
    }

    /// Dispatch a mutation. Assigns self to the new value and returns it if successful.
    /// 
    /// This will dispatch `Update::Mutate` to all subscribed async tasks if successful.
    pub fn mutate(&mut self, mutn: V::Mutation) -> Result<Entry<V>, V::MutError> {
        let new_val = Arc::new(V::mutate(&self.as_entry(), &mutn)?);
        let entry = {
            let mut raw = self.store.map.get_mut(&self.uuid)
                .expect("[E889] Make a github issue if you see this.");
            raw.change(new_val.clone(), None, Some(mutn));
            Entry::from_raw(&raw)
        };
        self.value = new_val;
        self.create_time = entry.create_time;
        self.last_change = entry.last_change;
        Ok(entry)
    }

    /// Delete the entry from the map.
    /// 
    /// This will dispatch a `Update::Remove` to all subscribed async tasks.
    pub fn delete(self) {
        self.store.map.remove_if(&self.uuid, |uuid, raw| {
            let _ = raw.broadcast.send(
                Update::Remove {
                    val: raw.value.clone(),
                    msg: None,
                    key: *uuid,
                }
            );
            true
        });
    }

    /// Delete the entry from the map with an attached message.
    /// 
    /// This will dispatch an `Update::Removed` to all subscribed async tasks.
    pub fn delete_with_msg(self, msg: V::Message) {
        self.store.map.remove_if(&self.uuid, |uuid, raw| {
            let _ = raw.broadcast.send(
                Update::Remove {
                    val: raw.value.clone(),
                    msg: Some(msg),
                    key: *uuid,
                }
            );
            true
        });
    }
}

impl<V: LiveValue> Deref for EntryGuard<V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<V: LiveValue> Watch<V> {
    /// Await the next update from the receiver. 
    /// If the corresponding sender has closed, this will
    /// return `None`. 
    pub async fn next(&mut self) -> Option<Update<V>> {
        loop {
            match self.rx.recv().await {
                Ok(u) => return Some(u),
                Err(e) => {
                    match e {
                        RecvError::Closed => return None,
                        RecvError::Lagged(amt) => {
                            eprintln!(
                                "A receiver lagged by '{amt}' messages. Consider increasing the channel capacity for live type '{}'", 
                                type_name::<V>()
                            );
                            continue
                        },
                    }
                }
            }
        }
    }
}

impl<V: LiveValue> StoreType for LiveStore<V> { }

fn panic_uninit_value_type<T: Any>() -> ! {
    panic!("
        --- LIVESTORE VALUE ERROR CODE: 391 ---
        Attempted to get, set, or subscribe to a value of type '{}' into a LiveStore, but it had not been initialized.
        Value types needs to be initialized in with [`LiveStoreBuilder::add_val_store::<V>`] before they can be used. 
    ", type_name::<T>())
}

pub struct AsyncCallback<In, Out=()>(Arc<dyn Fn(In) -> Pin<Box<dyn Future<Output=Out> + Send + Sync>> + Send + Sync + 'static>);

impl<In, Out> Clone for AsyncCallback<In, Out> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<In, Out> AsyncCallback<In, Out> {
    pub async fn call(&self, input: In) -> Out {
        (self.0)(input).await
    }
}

impl<In, Out, F, Fut> From<F> for AsyncCallback<In, Out> 
where
    F: Fn(In) -> Fut + Send + Sync + 'static,
    Fut: Future<Output=Out> + Send + Sync + 'static,
{
    fn from(value: F) -> Self {
        Self(Arc::new(move |input| Box::pin(value(input))))
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::{Arc, Mutex}, time::Duration};

    use uuid::Uuid;

    use crate::{Entry, ExpireMode, LiveStore, LiveValue, StoreOptions, Update};

    #[derive(Clone, PartialEq, Eq, Debug)]
    struct Foo {
        bar: u32,
        baz: u64,
    }

    #[derive(Clone)]
    enum FooMutation {
        SetBar(u32),
        SetBaz(u64),
    }

    impl LiveValue for Foo {
        type Message = ();
        type MutError = ();
        type Mutation = FooMutation;

        fn mutate(entry: &crate::Entry<Self>, mutation: &Self::Mutation) -> Result<Self, Self::MutError> {
            match *mutation {
                FooMutation::SetBar(bar) => Ok(Self { bar, ..*entry.clone() }),
                FooMutation::SetBaz(baz) => Ok(Self { baz, ..*entry.clone() })
            }
        }
    }

    #[tokio::test]
    async fn mutate() {
        let store = LiveStore::<Foo>::default();
        let key = store.init(Foo { bar: 0, baz: 1 });
        store.mutate(&key, FooMutation::SetBar(6)).await.unwrap().unwrap();
        store.mutate(&key, FooMutation::SetBaz(9)).await.unwrap().unwrap();
        let val = store.remove(&key).await.unwrap();
        assert_eq!(val.bar, 6);
        assert_eq!(val.baz, 9);
    }

    #[tokio::test]
    async fn subscribe() {
        let store = LiveStore::<Foo>::default();
        let key = store.init(Foo { bar: 0, baz: 1 });
        let mut watch = store.watch(&key).unwrap();

        let lock1 = Arc::new(Mutex::new(Foo { bar: 0, baz: 0 }));
        let lock2 = lock1.clone();
        
        let rx_task = tokio::spawn(async move {
            while let Some(update) = watch.next().await {
                if let Update::Mutate { mutn, .. } = update {
                    match mutn {
                        FooMutation::SetBar(bar) => lock2.lock().unwrap().bar = bar,
                        FooMutation::SetBaz(baz) => lock2.lock().unwrap().baz = baz,
                    }
                }
            }
        });

        store.mutate(&key, FooMutation::SetBar(6)).await.unwrap().unwrap();
        store.mutate(&key, FooMutation::SetBaz(9)).await.unwrap().unwrap();
        let val = store.remove(&key).await.unwrap();
        rx_task.await.unwrap();

        assert_eq!(val.bar, 6);
        assert_eq!(val.baz, 9);
        assert_eq!(lock1.lock().unwrap().bar, 6);
        assert_eq!(lock1.lock().unwrap().baz, 9);
        assert!(store.fetch(&key).is_none());
    }

    #[tokio::test]
    async fn on_expire() {
        let expired = Arc::new(Mutex::new(Vec::new()));
        let expired2 = expired.clone();

        let store = LiveStore::<Foo>::new()
            .with_options(
                StoreOptions {
                    expire_check_freq: Some(Duration::from_secs(1)),
                    expire_mode: ExpireMode::Timeout(Duration::from_millis(1)),
                    ..Default::default()
                }
            )
            .on_expire(move |buf: Vec<(Uuid, Entry<Foo>)>| {
                let expired = expired2.clone();
                async move {
                    expired.lock().unwrap().extend(buf.iter().map(|(_, item)| item.value.as_ref().clone()));
                }
            }).done();

        let f1 = Foo { bar: 0, baz: 1 };
        let f2 = Foo { bar: 1, baz: 2 };
        let f3 = Foo { bar: 2, baz: 3 };

        let k1 = store.init(f1.clone());
        let k2 = store.init(f2.clone());
        let k3 = store.init(f3.clone());

        tokio::time::sleep(Duration::from_secs(2)).await;

        assert!(store.fetch(&k1).is_none());
        assert!(store.fetch(&k2).is_none());
        assert!(store.fetch(&k3).is_none());

        let guard = expired.lock().unwrap();

        for foo in [f1, f2, f3] {
            if !guard.contains(&foo) {
                panic!("{:?}, {:?}", foo, guard)
            }
        }
    }
}