
#![doc = include_str!("../README.md")]

use std::{any::{type_name, Any, TypeId}, ops::Deref, pin::Pin, sync::Arc, time::{Duration, Instant}};

use dashmap::{mapref::one::{Ref, RefMut}, DashMap};
use tokio::sync::{broadcast::{error::RecvError, Receiver, Sender}, Mutex, OwnedMutexGuard};
use uuid::Uuid;

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
/// This function will spawn a tokio task when `on_update` is called
/// that will poll for updates. If you'd rather handle the receiver
/// yourself, you can do so with the public `rx` field.
pub struct Watch<V: LiveValue> {
    pub rx: Receiver<Update<V>>,
}

/// Used to control the behavior of [`Watch`] subscriptions
/// from within the polling loop.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Control {
    Continue,
    Break
}

impl Storages {
    pub fn new() -> StoragesBuilder {
        StoragesBuilder {
            stores: Vec::new(),
        }
    }

    /// Get a reference to the ValueStore for a specific type.
    /// 
    /// This function will panic if [`LiveStoreBuilder::add_store::<V>`] has not been called.
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
    pub fn new(options: StoreOptions) -> Arc<Self> {
        Arc::new(
            Self {
                map: options.shard_amt.map(|amt| DashMap::with_shard_amount(amt)).unwrap_or_default(),
                options,
            }
        )
    }

    /// Set the expiration callback
    pub fn on_expire<F, Fut>(self: &Arc<Self>, cb: F) 
    where
        F: Fn(Vec<(Uuid, Entry<V>)>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output=()> + Send + Sync + 'static,
    {
        let store = self.clone();
        let mode = self.options.expire_mode;
    
        if let Some(freq) = self.options.expire_check_freq {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(freq);
                loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => return,
                        _ = interval.tick() => {
                            let mut expired = Vec::new();

                            match mode {
                                ExpireMode::Timeout(dur) => {
                                    let min = Instant::now() - dur;
            
                                    store.map.retain(|uuid, raw| {
                                        if raw.last_change < min {
                                            let _ = raw.broadcast.send(
                                                Update::Remove {
                                                    msg: None,
                                                    val: raw.value.clone(),
                                                    key: *uuid,
                                                }
                                            );
                                            expired.push((*uuid, Entry::from_raw(&raw)));
                                            false
                                        } else {
                                            true
                                        }
                                    });
                                }
                            }

                            cb(expired).await;
                        },
                    }
                }
            });
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
    pub async fn next(&mut self) -> Option<Update<V>> {
        loop {
            match self.rx.recv().await {
                Ok(u) => return Some(u),
                Err(e) => {
                    match e {
                        RecvError::Closed => return None,
                        RecvError::Lagged(amt) => {
                            eprintln!(
                                "A receiver lagged by '{amt}' messages. Consider increasing the channel capacity for '{}'", 
                                type_name::<V>()
                            );
                            continue
                        },
                    }
                }
            }
        }
    }

    /// Watch for changes to the subscribed value.
    /// The provided closure will run whenever the value is updated.
    ///
    /// To keep watching for changes, use `Control::Break` to close the 
    /// receiver.
    pub fn on_update<F, Fut>(mut self, mut f: F) 
    where
        F: FnMut(Update<V>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output=Control> + Send + Sync + 'static
    {
        tokio::spawn(async move {
            loop {
                match self.rx.recv().await {
                    Ok(update) => {
                        if let Control::Break = (f)(update).await {
                            return;
                        }
                    },
                    Err(RecvError::Closed) => return,
                    _ => {}
                }
            }
        });
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{Control, LiveStore, LiveValue, StoreOptions, Update};

    #[derive(Clone)]
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

    #[test]
    fn subscribe() {
        let store = LiveStore::<Foo>::new(StoreOptions::default());
        let key = store.init(Foo { bar: 0, baz: 1 });
        let sub = store.watch(&key).unwrap();

        let bar_lock = Arc::new(Mutex::new(0));
        let baz_lock = Arc::new(Mutex::new(0));

        let bar_lock2 = bar_lock.clone();
        let baz_lock2 = baz_lock.clone();

        sub.on_update(async move |update| {
            if let Update::Mutate { mutn, .. } = update {
                match mutn {
                    FooMutation::SetBar(bar) => *bar_lock2.lock().unwrap() = bar,
                    FooMutation::SetBaz(baz) => *baz_lock2.lock().unwrap() = baz,
                }
            }

            Control::Continue
        });
    }
}