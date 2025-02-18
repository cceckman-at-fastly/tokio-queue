# Investigating Tokio I/O responsiveness

We think Tokio to hold this invariant:
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

## Tokio idleness

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

Per discussion with @aturon:

If all threads are idle, Tokio performs a blocking `epoll` call to wait for new work.
If there's new work, that thread might handle it... but it won't necessarily wake other threads until it's done with its work.

This **can lead to poor I/O responsiveness** on a transition from "little work" to "lots of work".
The first work item will wake a worker, but it won't check for additional work (i.e. check I/O) until that first work unit is done.
It's like "A Decade of Wasted Cores": even though there is work available (waiting on I/O), all the threads are still asleep.


