Provides a read-copy-update locking primitive

RCU locks are a popular concurrency primitive inside the linux kernel. They support multiple
concurrent readers accessing a value updated by a single writer. RCU locks are useful for
publishing new configuration values to multiple readers or as a lightweight garbage collection
mechanism.

# Example

To create a new RCU lock, call [`Rcu::new`] with an initial value. Then create a new [`Reader`]
by calling [`Rcu::reader`], this provides a handle which allows accessing a value by calling
[`Reader::read`].

```
# use read_copy_update::Rcu;
# use std::{thread, time::Duration};
let mut rcu = Rcu::new(10);

// This thread can read the value, new updates will be published without requiring locks.
let mut rdr = rcu.reader();
thread::spawn(move || {
    println!("{}", *rdr.read()); // prints '10'
    thread::sleep(Duration::from_millis(1));
    println!("{}", *rdr.read()); // prints '20'
});

// The value can be updated on the main thread, independently of other readers.
rcu.write(20);
```

A common use-case is sharing readers in thread-local storage. With the `thread-local` feature
flag enabled [`Reader`] instances can be associated to a thread allowing lock free reads and
publishing across thread-locals.

```
# use read_copy_update::{Rcu, ThreadLocal};
# use std::{thread, time::Duration};
let mut rcu = Rcu::new(10);
let tls = ThreadLocal::new(&rcu);

thread::scope(|s| {
    s.spawn(|| {
        let val = tls.get_or_init();
        println!("{}", *val);
    });
    s.spawn(|| {
        let val = tls.get_or_init();
        println!("{}", *val);
    });
});

// The value can be updated on the main thread, independently of other readers.
rcu.write(20);
```

# Design

Internally, new values are allocated on the heap and associated with an epoch. Readers pull in
the pointer on read() calls and increment a per-reader epoch counter. Reclamation is done based
on ensuring that all active readers have advanced their epoch past the value to be reclaimed.
Because of this design, nested read() calls will not pick up the latest value, only once all
references are dropped will the value be refreshed.
