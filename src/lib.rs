#![doc = include_str!("../readme.md")]

use std::{collections::VecDeque, ops::Deref, ptr};

#[cfg(not(loom))]
use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicPtr, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

#[cfg(loom)]
use loom::{
    cell::Cell,
    sync::{
        atomic::{AtomicPtr, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

/// Represents a read-copy-update for a specific value.
///
/// This is the write side, new read handles can be constructed by calling [`Rcu::reader`].
#[derive(Debug)]
pub struct Rcu<T> {
    epoch: u64,
    shared: Arc<Shared<T>>,
}

/// The reader handle for a value stored in an [`Rcu`].
///
/// Specific values can be read using [`Reader::read`]. Readers are `!Sync` and expected to be used
/// only on a single thread.
#[derive(Debug)]
pub struct Reader<T: 'static> {
    cache: Cell<&'static StampedValue<T>>,
    refs: Cell<usize>,
    state: ReaderState,
    shared: Arc<Shared<T>>,
}

#[derive(Debug)]
struct Shared<T> {
    ptr: Pointer<T>,
    reclaim: Mutex<VecDeque<Box<StampedValue<T>>>>,
    readers: Mutex<Vec<ReaderState>>,
}

#[derive(Debug)]
struct StampedValue<T> {
    value: T,
    epoch: u64,
}

#[derive(Debug, Clone)]
struct ReaderState(Arc<AtomicU64>);

#[derive(Debug)]
struct Pointer<T>(AtomicPtr<StampedValue<T>>);

#[derive(Debug)]
pub struct Guard<'a, T: 'static> {
    cache: &'a StampedValue<T>,
    reader: &'a Reader<T>,
}

impl<T: 'static> Default for Rcu<T>
where
    T: Default,
{
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: 'static> Rcu<T> {
    /// Constructs a new RCU with an initial value.
    pub fn new(value: T) -> Self {
        Self {
            epoch: 1,
            shared: Arc::new(Shared {
                ptr: Pointer::new(StampedValue { value, epoch: 1 }),
                reclaim: Mutex::new(VecDeque::new()),
                readers: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Registers a new [`Reader`] allowing values to be read.
    pub fn reader(&mut self) -> Reader<T> {
        Reader::new(self.shared.clone())
    }

    /// Write a new value making it available to all readers.
    ///
    /// Previously written values will be reclaimed when they are no longer accesed.
    pub fn write(&mut self, value: T) {
        // Records a new epoch associated with this value, not allowed to wrap around.
        self.epoch += 1;

        // Publish the value, we will attempt to reclaim the previous value.
        let next = StampedValue {
            epoch: self.epoch,
            value,
        };
        let prev = self.shared.ptr.swap(next);
        self.reclaim_queue().push_back(prev);

        // Immediately attempt to reclaim.
        self.try_reclaim();
    }

    /// Try and reclaim any values which are no longer in-use.
    ///
    /// Returns the number of values still waiting to be reclaimed.
    pub fn try_reclaim(&mut self) -> usize {
        let mut readers = self.shared.readers.lock().unwrap();

        // Trim readers which have been removed.
        readers.retain(|reader| reader.get() > ReaderState::NOT_IN_USE);

        let mut reclaim = self.reclaim_queue();

        // If there are no readers, we can reclaim everything.
        if readers.is_empty() {
            reclaim.clear();
        }

        // Check the minimum epoch across all active threads, removing records
        // that are below the minimum epoch.
        let min_epoch = readers
            .iter()
            .map(|r| r.get())
            .min()
            .unwrap_or(ReaderState::NOT_IN_USE);
        while let Some(candidate) = reclaim.pop_front() {
            if min_epoch > candidate.epoch {
                drop(candidate);
            } else {
                reclaim.push_front(candidate);
                // We short circuit, no point checking others with a higher epoch.
                return reclaim.len();
            }
        }
        0
    }

    fn reclaim_queue(&self) -> MutexGuard<'_, VecDeque<Box<StampedValue<T>>>> {
        // The reclaimer must be shared so we can drop any remaining values when the 'Arc<Shared>'
        // drops, but access to it should only be from this function. As a result we protect it with
        // a mutex but only rely on try_lock().
        self.shared
            .reclaim
            .try_lock()
            .expect("invalid shared reclaimer access")
    }
}

impl<T: 'static> Reader<T> {
    fn new(shared: Arc<Shared<T>>) -> Self {
        let value = shared.ptr.load();
        let mut readers = shared.readers.lock().unwrap();

        let state = ReaderState::new(value.epoch);
        readers.push(state.clone());

        Reader {
            shared: shared.clone(),
            refs: Cell::new(0),
            cache: Cell::new(value),
            state,
        }
    }

    /// Reads the latest value guarded to ensure that the pointer will not be reclaimed while the
    /// current reader has access.
    pub fn read(&self) -> Guard<'_, T> {
        // The read method provides a guard that allows deref access to one of the values written
        // by the writer previously. The invariant this method maintains, using ref-counts, is that
        // the epoch stamped on the current thread is always less than or equal to the epoch of the
        // last used value. As soon as the reclaimer sees an epoch for a specific thread, it can be
        // sure that no references with epochs 'below' the available epoch exist on that thread.

        let cache = if self.refs.get() == 0 {
            let value = self.shared.ptr.load();

            // Update the epoch to note that we are currently using this value. This uses release
            // ordering to ensure that loads when reclaiming will be ordered after this operation.
            self.state.set(value.epoch);

            // Cache the pointer in the current reader.
            self.cache.set(value);
            value
        } else {
            self.cache.get()
        };

        self.refs.set(self.refs.get() + 1);

        Guard {
            reader: self,
            cache,
        }
    }
}

impl<'a, T> Deref for Guard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.cache.value
    }
}

impl<'a, T> Drop for Guard<'a, T> {
    fn drop(&mut self) {
        self.reader.refs.replace(self.reader.refs.get() - 1);
    }
}

impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        self.state.mark_dropped();
    }
}

impl<T> Pointer<T> {
    fn new(value: StampedValue<T>) -> Self {
        Self(AtomicPtr::new(Box::leak(Box::new(value))))
    }

    fn swap(&self, value: StampedValue<T>) -> Box<StampedValue<T>> {
        let ptr = Box::leak(Box::new(value));
        let prev = self.0.swap(ptr, Ordering::AcqRel);
        unsafe { Box::from_raw(prev) }
    }

    fn load(&self) -> &'static StampedValue<T> {
        unsafe { &*self.0.load(Ordering::Relaxed) }
    }
}

impl<T> Drop for Pointer<T> {
    fn drop(&mut self) {
        let prev = self.0.swap(ptr::null_mut(), Ordering::AcqRel);
        let _ = unsafe { Box::from_raw(prev) };
    }
}

impl ReaderState {
    const NOT_IN_USE: u64 = 0;

    fn new(epoch: u64) -> Self {
        Self(Arc::new(AtomicU64::new(epoch)))
    }

    fn mark_dropped(&self) {
        self.set(Self::NOT_IN_USE)
    }

    fn set(&self, epoch: u64) {
        self.0.store(epoch, Ordering::Release)
    }

    fn get(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// Provides thread-local storage to read [`Rcu`] values.
///
/// When a new thread is initialized a new [`Reader`] will be created and stored in a slot for the
/// provided thread. Values will be published to the thread-local and access will be cheap
#[cfg(feature = "thread-local")]
pub struct ThreadLocal<T: Send + Sync + 'static> {
    shared: Arc<Shared<T>>,
    thread_local: thread_local::ThreadLocal<Reader<T>>,
}

#[cfg(feature = "thread-local")]
impl<T: Send + Sync + 'static> ThreadLocal<T> {
    pub fn new(rcu: &Rcu<T>) -> Self {
        Self {
            shared: rcu.shared.clone(),
            thread_local: thread_local::ThreadLocal::new(),
        }
    }

    /// Returns the element for the current thread, if it exists,
    pub fn get(&self) -> Option<Guard<'_, T>> {
        self.thread_local.get().map(|r| r.read())
    }

    /// Returns the element for the current thread, or creates it if it doesn't exist.
    pub fn get_or_init(&self) -> Guard<'_, T> {
        self.thread_local
            .get_or(|| Reader::new(self.shared.clone()))
            .read()
    }
}

#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
    use loom::thread;

    use super::*;

    #[test]
    fn nested() {
        loom::model(|| {
            let mut rcu = Rcu::new(10);

            let rdr = rcu.reader();

            {
                let g = rdr.read();
                assert_eq!(10, *g);

                rcu.write(20);
                {
                    let g = rdr.read();
                    assert_eq!(10, *g);
                }
            }
        });
    }

    #[test]
    fn thread_nested() {
        loom::model(|| {
            let n = 2;
            let mut rcu = Rcu::new(0);
            let rdr = rcu.reader();
            let h = thread::spawn(move || {
                let v = rdr.read();
                assert!(*v < n);

                {
                    let g = rdr.read();
                    assert!(*g < n);
                }
            });
            for i in 0..n {
                rcu.write(i);
                loom::thread::yield_now();
            }
            h.join().unwrap();
        });
    }

    #[test]
    fn thread() {
        loom::model(|| {
            let n = 2;
            let mut rcu = Rcu::new(0);
            let rdr = rcu.reader();
            let h = thread::spawn(move || {
                for _ in 0..n {
                    let v = rdr.read();
                    assert!(*v < n);
                    loom::thread::yield_now();
                }
            });
            for i in 0..n {
                rcu.write(i);
                loom::thread::yield_now();
            }
            h.join().unwrap();
        });
    }

    #[test]
    fn thread_detached() {
        loom::model(|| {
            let n = 2;
            let mut rcu = Rcu::new(0);
            let rdr = rcu.reader();
            thread::spawn(move || {
                for _ in 0..n {
                    let v = rdr.read();
                    assert!(*v < n);
                    loom::thread::yield_now();
                }
            });
            for i in 0..n {
                rcu.write(i);
                loom::thread::yield_now();
            }
        });
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::{
        sync::{atomic::AtomicUsize, Condvar},
        thread,
        time::Duration,
    };

    use super::*;

    thread_local! {
        static REFS: AtomicUsize = AtomicUsize::new(0);
    }

    struct RefsCheck;

    impl RefsCheck {
        fn new() -> Self {
            REFS.with(|refs| {
                assert_eq!(refs.load(Ordering::SeqCst), 0);
            });
            Self
        }
    }

    impl Drop for RefsCheck {
        fn drop(&mut self) {
            REFS.with(|refs| {
                assert_eq!(refs.load(Ordering::SeqCst), 0);
            });
        }
    }

    #[derive(Debug)]
    struct RecordDrop(u32);

    impl RecordDrop {
        fn new(v: u32) -> Self {
            REFS.with(|refs| {
                refs.fetch_add(1, Ordering::SeqCst);
            });
            Self(v)
        }
    }

    impl Drop for RecordDrop {
        fn drop(&mut self) {
            REFS.with(|refs| {
                refs.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }

    #[cfg(feature = "thread-local")]
    #[test]
    fn thread_local() {
        let mut rcu = Rcu::new(10);
        let tls = ThreadLocal::new(&rcu);

        thread::scope(|s| {
            s.spawn(|| {
                let _val = tls.get_or_init();
                assert!(tls.get().is_some());
            });
            s.spawn(|| {
                let _val = tls.get_or_init();
                assert!(tls.get().is_some());
            });
        });

        rcu.write(1);
    }

    #[test]
    fn send_check() {
        let mut rcu = Rcu::new(10);
        let rdr = rcu.reader();

        thread::spawn(move || {
            assert_eq!(10, *rdr.read());
        });
    }

    #[test]
    fn single_value() {
        let _refs = RefsCheck::new();

        let mut rcu = Rcu::new(RecordDrop::new(10));
        let rdr = rcu.reader();
        assert_eq!(10, rdr.read().0);
    }

    #[test]
    fn old_value() {
        let _refs = RefsCheck::new();

        let mut rcu = Rcu::new(RecordDrop::new(10));
        let rdr1 = rcu.reader();
        assert_eq!(10, rdr1.read().0);

        let rdr2 = rcu.reader();
        assert_eq!(10, rdr2.read().0);

        for i in 11..=20 {
            rcu.write(RecordDrop::new(i));
            assert_eq!(i, rdr1.read().0);
        }

        // because of the limitations of the current design, all values will
        // not be dropped until this point.
    }

    #[test]
    fn remove_readers() {
        let _refs = RefsCheck::new();

        let mut rcu = Rcu::new(RecordDrop::new(10));

        let rdr1 = rcu.reader();
        let rdr2 = rcu.reader();

        for i in 11..=20 {
            rcu.write(RecordDrop::new(i));
        }

        drop(rdr1);
        drop(rdr2);

        rcu.write(RecordDrop::new(30));
    }

    #[test]
    fn nested() {
        let _refs = RefsCheck::new();

        let mut rcu = Rcu::new(RecordDrop::new(10));

        let rdr = rcu.reader();

        {
            let handle = rdr.read();
            assert_eq!(10, handle.0);

            rcu.write(RecordDrop::new(20));

            {
                let handle = rdr.read();
                assert_eq!(10, handle.0);
            }

            let handle2 = rdr.read();
            assert_eq!(10, handle.0);
            assert_eq!(10, handle2.0);
        }

        assert_eq!(20, rdr.read().0);
    }

    #[test]
    fn nested_multi_threaded() {
        let _refs = RefsCheck::new();

        let notify = Arc::new((Mutex::new(false), Condvar::new()));

        let mut rcu = Rcu::new(RecordDrop::new(10));
        let rdr = rcu.reader();
        assert_eq!(10, rdr.read().0);

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let rdr = rcu.reader();
                let notify = notify.clone();
                std::thread::spawn(move || {
                    let _refs = RefsCheck::new();

                    assert_eq!(10, rdr.read().0);
                    {
                        let handle = rdr.read();
                        assert_eq!(10, handle.0);
                    }

                    let (lock, cvar) = &*notify;
                    let mut started = lock.lock().unwrap();
                    while !*started {
                        started = cvar.wait(started).unwrap();
                    }

                    assert_eq!(20, rdr.read().0);
                    {
                        let handle = rdr.read();
                        assert_eq!(20, handle.0);
                    }
                })
            })
            .collect();

        thread::sleep(Duration::from_millis(10));
        rcu.write(RecordDrop::new(20));

        {
            let (lock, cvar) = &*notify;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        handles.into_iter().for_each(|h| h.join().unwrap());
    }
}
