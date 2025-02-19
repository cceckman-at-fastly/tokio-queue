# Investigating Tokio I/O responsiveness

I (we?) think Tokio to hold this invariant:
- Either **all tasks are busy**, actively working on Futures; or
- **A thread is blocked on I/O**, ready to respond immediately as new work arrives.

Without this invariant, new work from I/O will be ignored **even when there are available threads to serve it**.

## Starting point

While working on [Fractal Overdrive](https://cceckman.com/writing/fractal-overdrive/), I saw some odd behavior from Tokio+Axum.

The original (spring 2024) interface was a Tokio+Axum powered webserver.
The server implemented some "generate fractal" request handlers; it would render the fractal
in the server thread and return an image from the request handler.
This meant the pool was heavily CPU-bound.

I noticed a strange queueing behavior when loading the page. One image would load first,
then others would load concurrently.
Looking at the network timeline in my browser, I saw the same behavior: with a threadpool of (say) 4,
first one image would load, then ~4 would load approximately at the same time, and after about
twice as long as the first image.

When I dug further into the logs, the server side confirmed this as well.
The first request was served before work started on the others. What gives?

## Reproduction

I reproduced this in this repository. Run `cargo run --bin original`, open `localhost:3000` in your browser, and watch.

The program starts up an Axum webserver, backed by a Tokio pool with four workers.

### Browser

The page loads five images; each of which is constrained to take two seconds to "render"
(really, just sleeps). The web browser dispatches all the requests at the same time.

The first loads immediately. The other four load strictly after the first. Specifically,
the first takes 2001 milliseconds to load, and the other take just shy of 4000 milliseconds to load.

We see this corroborated on the server side. The server logs:

```
2025-02-18T20:03:43.096578Z  INFO original:   parked: 4 parked
2025-02-18T20:03:43.131159Z  INFO original: unparked: 3 parked
2025-02-18T20:03:43.131434Z  INFO original: starting image: 1
2025-02-18T20:03:45.131691Z  INFO original: finishing image response
```

without "noticing" that there are additional requests present.

Once that is done, the server wakes additional threads and starts processing:

```
2025-02-18T20:04:32.845289Z  INFO original: finishing image response
2025-02-18T20:04:32.845611Z  INFO original:   parked: 4 parked
2025-02-18T20:04:32.845709Z  INFO original: unparked: 3 parked
2025-02-18T20:04:32.845834Z  INFO original: unparked: 2 parked
2025-02-18T20:04:32.846002Z  INFO original: unparked: 1 parked
2025-02-18T20:04:32.846077Z  INFO original: starting image: 5
2025-02-18T20:04:32.846129Z  INFO original: unparked: 0 parked
2025-02-18T20:04:32.846145Z  INFO original: starting image: 3
2025-02-18T20:04:32.846262Z  INFO original: starting image: 4
2025-02-18T20:04:32.846343Z  INFO original: starting image: 2
```

### Client

`client_http1` reproduces the issue.

- The client connects (with one connection) and sends a first request.
- Once that request is complete, the client sends five more requests:
  - One on the existing (open) connection
  - Four by creating new connections and sending the request

The results:

```
starting prewarm request...
prewarm done
query 0: 2001
query 1: 4002
query 2: 4002
query 3: 4002
query 4: 4002
```

This behavior is observed with `multi_thread` and `single_thread` Tokio executors on the client side.

### Non-reproductions

-   An earlier version of `client_http1` made all the connections serially, then made all the requests.
    This was not subject to the queueing at issue here -- the first four requests all completed in ~2 seconds, as expected with a 4-worker server.
-   `client_http2`, which multiplexes all the requests on the same HTTP/2 stream, does not reproduce this behavior.

## What's going on?

Let's start with the `TcpListener`, as that's where the request comes in.
It implements an `async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)>`,
i.e. a point at which a future can `await` a new TCP connection.

It is passed to `axum::serve`, which eventually winds up as a future in
`axum::serve::WithGracefulShutdown::into_future`. The inner `ServeFuture` there
repeatedly:

1.  Awaits `listener.accept()`, and maps it to a common I/O trait
2.  Makes a new `TowerToHyperService` for the conection
3.  `tokio::spawn`s a new task

    In that task, it connects the I/O and Hyper service (`builder`) and runs them;
    the combined object is a future that runs all requests for the connection.

Some observations:

- There may be at most two tasks runnable at any time.
  If no second connection comes through, the accepter thread may be slept
  awaiting the socket to be readable; we may wind up with only one runnable task.


## `block_on`

Let's look more deeply at how Tokio will handle scheduling.

We created a `Runtime` to begin with and called `block_on`, which notes:

> This runs the given future on the current thread, blocking until it is
> complete, and yielding its resolved result. Any tasks or timers
> which the future spawns internally will be executed on the runtime.
> ...
> When the multi thread scheduler is used this will allow futures
> to run within the io driver and timer context of the overall runtime.
> Any spawned tasks will continue running after `block_on` returns.

So, this particular future -- the one `accept`ing -- will run on the current thread.
Other futures *may* run on other threads. (Will this future not get rescheduled to another thread?
There's no `Send` constraint on the future -- so, I guess not.)

Might that be the problem? Once this thread "decides" to run on the local thread,
if the local thread starts picking up other work -- e.g. HTTP responses --
the `accept`ing thread won't run any more? Let's try, with the example in `spawn_pool.rs`:
we'll `spawn` the `serve` future (which requires it be `Send`) to allow it to migrate
around the pool.

Still, no dice! Same behavior as before.

## `spawn`

What hapens to that spawned future?

IN this case, it winds up going down to `tokio::runtime::scheduler::multi_thread::Handle::bind_new_task`.
That does three calls:
- `worker::OwnedTasks::bind`, generating the `JoinHandle`; this is an executor-wide database.
  `OwnedTasks::bind` also calls to `runtime::task::new_task`, where the `Task` structure is created.
- `TaskHooks::spawn`, providing just the task ID
  This invokes - some sort of callback. It looks like they're meant to be user-provided, but can't be today.

- `Handle::schedule_option_task_without_yield`

  This in turn calls `Handle::schedule_task`, if the task was "notified" to begin with
  (Not sure what that means; "already ready to run"? See `new_task`.)

`schedule_task` is where the magic happens. Its logic:

```rust
            if let Some(cx) = maybe_cx {
                // Make sure the task is part of the **current** scheduler.
                if self.ptr_eq(&cx.worker.handle) {
                    // And the current thread still holds a core
                    if let Some(core) = cx.core.borrow_mut().as_mut() {
                        self.schedule_local(core, task, is_yield);
                        return;
                    }
                }
            }

            // Otherwise, use the inject queue.
            self.push_remote_task(task);
            self.notify_parked_remote();
```

"If the current thread holds a core" (i.e. has claimed a worker state), `schedule_local`;
otherwise, push to the global queue and notify parked remotes.
I guess this is what lets us `spawn` from outside the runtime- the current thread won't have a core.

In cases where the LIFO queue is not used (including when it is already full),
`schedule_local` will `push_back_or_overflow` into the local `run_queue`, which marks `should_notify`.

If `should_notify` (i.e. there is a task in our queue) and a `Parker` is available on the `Core`, it then calls `notify_parked_local`.
That picks an `idle.worker_to_notify` and unparks it.

This `schedule_local` code includes this comment before notifying:

```rust
        // Only notify if not currently parked. If `park` is `None`, then the
        // scheduling is from a resource driver. As notifications often come in
        // batches, the notification is delayed until the park is complete.
        if should_notify && core.park.is_some() {
            self.notify_parked_local();
        }
```

- [ ] I'm not sure I understand that.

## More stuff to understand

`is_searching` state in `Worker`.

Let's start from `run(Arc<Worker>)`.
1.  Try to get a `core`; exit if you can't claim it.
    So: for everything else, assume we have a `core`.
2.  Create a multi-threaded handle.
3.  `enter_runtime(handle, allow_block_in_place: true, closure)`, which:
    1.  Checks that we aren't yet part of a runtime, via the `CONTEXT` TLS
    2.  Creates an EnterRuntimeGuard
    3.  In the closure, creates a new scheduler context
    4.  In the closure, `set_scheduler`, which binds that new scheduler context to the `CONTEXT`
    5.  Runs `Context::run(core)` with that thread-local state set
    6.  On exit, may wake deferred tasks (?) in the scheduler context.
4.  `Context::run(core)`:
    ...now we're getting somewhere?

    Loops around "not shutdown";
    1.  Ticks
    2.  Performs maintenance, on every N ticks:

        > "Enables the i/O driver, timer, ... to run without actually putting the thread to sleep"

        So `Context::park_timeout` includes invoking the event-trigger checks?
        - [ ] Read it later

        Internal "Run scheduled maintenance"; update shutdown, update tracing, publish stats.
        Stuff that requires cross-thread locks, I guess?
    3.  Look for the next local task: `Core::next_task`
        1.  On a "global queue interval", retune that same interval...
            and take a task from the global queue, if available.

            - [ ] Checking understanding: this is to ensure that the global queue is eventually drained.
              If all the threads remain busy with their local work, we could conceivably be in a place
              where stuff in the global queue never gets drained.

        2.  Take a local task, if one is available.
        3.  Pull from the injection queue, if available, into our local queue.

            - [ ] When does work go into the injection queue?
    4.  If there is no local work, or work in the injection queue, or work in the global queue,
        `Core::steal_work`.

        1.  `transition_to_searching`. This involves synchronizing on a couple of atomics,
            in an attempt to limit the number of workers searching at a time.

            This may fail (say "no you shouldn't be searching"), in which case the thread remains with no work.
        2.  For up to the total number of workers, attempt to steal (half the tasks) from one of them
            with `Steal::Steal_into`.
        3.  Failing that, take from the global queue (`next_remote_task`).
    5.  If there is no global work either,
        if `Context::defer` is empty, `park`; otherwise, `park_timeout` with a timeout of 0.

        `Core::defer` is a queue(?) in the local context that has
        "Tasks to wake after resource drivers are polled. This is mostly to handle yielded tasks."


### Parking

Two entry points: `Context::park` and `Context::park_timeout`. `park` has this note:

```rust
    /// This function checks if indeed there's no more work left to be done before parking.
    /// Also important to notice that, before parking, the worker thread will try to take
    /// ownership of the Driver (IO/Time) and dispatch any events that might have fired.
    /// Whenever a worker thread executes the Driver loop, all waken tasks are scheduled
    /// in its own local queue until the queue saturates (ntasks > `LOCAL_QUEUE_CAPACITY`).
    /// When the local queue is saturated, the overflow tasks are added to the injection queue
    /// from where other workers can pick them up.
    /// Also, we rely on the workstealing algorithm to spread the tasks amongst workers
    /// after all the IOs get dispatched
```

This doesn't apear to happen for `park_timeout`, i.e. if there are already "to wake after I/O driver tasks"--
presumably because `park_timeout` completes immediately, and we know those threads will be waked.

The actual call to "try the driver" is in `Parker::park_timeout`: `self.inner.shared.driver.try_lock()`.

Oddly, `park_timeout` asserts that the duration is 0 (!) for this path.

`Inner::park` checks if already notified (in which case, it returns regardless).
Then, attempts to lock the I/O driver, and either parks on the driver, or parks on a condition variable.

# Speculation

When does Tokio wake a previously-idle thread?

Here's what I think is happening -- from memory:

Axum's task hierarchy is as follows:
- A task `accept`s connections on the listening socket(s), and spawns off connection tasks.
- A task runs per connection, waiting for input and parsing it into requests.
- A task runs per request, running user handlers.

This can lead to a situation where, with HTTP/1.1, new work gets ignored:

- User agent connects (connection A). `accept` task wakes, spawns connection task, sleeps on I/O.
- The user agent creates another several connections (Connections B-D) for parallel requests.
  This causes no userspace changes to scheduleability-- it is only obseved the next time epoll is called.
- Tokio returns to its scheduling loop, but may not check I/O or wake other threads:
  - There is a task ready (connection A)
  - There are not more tasks than threads (i.e. no concurrency to be gained by waking another thread)

- Connection A task is scheduled, and is immediately ready to read from the connection.
  It presumably reads a request, and spins off a new Future.

- Again, Tokio returns to its scheduler. As before, there is a single ready task for a single ready thread.
- Tokio executes the (CPU-bound) request, and produces a response.

- The task pool is exhausted, so Tokio performs an epoll. It observes the socket is has new connections pending,
  and wakes the acceptor.
- The acceptor connects _all_ connections and sleeps.
- Tokio sees there is more than one task, and there are more threads available -- so it wakes the other threads.
  From here, work proceeds in parallel.

## Bigger problems?

From discussion with @aturon:

If all threads are idle, Tokio performs a blocking `epoll` call to wait for new work.
If there's new work, that thread might handle it... but it won't necessarily wake other threads until it's done with its work.

This **can lead to poor I/O responsiveness** on a transition from "little work" to "lots of work".
The first work item will wake a worker, but it won't check for additional work (i.e. check I/O) until that first work unit is done.
It's like "A Decade of Wasted Cores": even though there is work available (waiting on I/O), all the threads are still asleep.


