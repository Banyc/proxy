//! Deterministic probe for the accept-queue handover on the pinned
//! `udp_listener`, driving the listener the way this workspace's accept paths
//! drive it. It prints observations and always exits 0: it is a probe, not a
//! gate, so a strand shows up as a printed `handover=deferred` line rather
//! than as a failure.
//!
//! The two properties it separates:
//!
//! - **Handover after cancellation.** The accept future is parked in its
//!   datagram read and then dropped (what a `select!` arm recreated every
//!   loop iteration does when another arm wins), and a datagram that opens a
//!   flow is dialled afterwards. `poll_next_conn` drains the accept queue
//!   with a synchronous `try_accept_next` before each read, so the next call
//!   observes the datagram and returns the flow. A strand needs a drop
//!   *between* the enqueue and the dequeue; at the pinned revisions the
//!   enqueue (`dispatch_next`'s `push_back`) and the dequeue
//!   (`try_accept_next`'s `pop_front`) are both synchronous, so no
//!   cancellation point exists between them for a single driver, and this
//!   case alone therefore cannot distinguish the two shapes.
//!
//! - **Handover to a concurrent dispatcher.** A flow enqueued by a *second*
//!   dispatcher on the same listener, while the combined accept loop is
//!   parked in a datagram read that never completes, is not observed by the
//!   combined loop until some later datagram arrives (`select!` selects only
//!   on the read). The split form used by `common::udp_runtime` awaits
//!   `accept_next` instead and hands the flow over. This is the only
//!   accept-handover defect the pinned revisions still exhibit, and it needs
//!   two concurrent dispatchers on one listener.
//!
//! Equivalent shapes in the pinned dependencies (same drain-before-await body,
//! single driver per listener): `rtp`'s `Listener::accept_next_conn`, and the
//! `poll_next_conn` of `udp_listener` from the tag that first carried the
//! handover fix (`v0.0.19`). The combined loop only gains the queue arm of the
//! wait in `udp_listener`'s `accept: wake the combined accept loop on a queued
//! flow`; a pin past that commit turns the deferred case below into a prompt
//! one, which is the only line this probe changes on a pin move.
//!
//! Run: `cargo run -p common --example accept_handover_probe`

use std::{
    future::Future,
    net::SocketAddr,
    num::NonZeroUsize,
    pin::Pin,
    task::{Context, Poll, Waker},
};

use udp_listener::{Packet, UtpListener};

/// Bounded poll budget for one handover observation. A deferred handover is
/// Pending for every poll, so the budget only bounds the probe's own
/// wall-clock; it is not a timing assertion.
const POLL_BUDGET: usize = 8;

type Listener = UtpListener<tokio::net::UdpSocket, SocketAddr, Packet>;
type Conn = udp_listener::Conn<tokio::net::UdpSocket, SocketAddr, Packet>;

fn poll_once<F: Future>(mut fut: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    fut.as_mut().poll(&mut cx)
}

/// Poll until Ready, yielding between polls so tokio's cooperative budget is
/// never the reason a poll returns Pending.
async fn poll_briefly<F: Future>(mut fut: Pin<&mut F>) -> Poll<F::Output> {
    for _ in 0..POLL_BUDGET {
        tokio::task::yield_now().await;
        let outcome = poll_once(fut.as_mut());
        if outcome.is_ready() {
            return outcome;
        }
    }
    Poll::Pending
}

async fn bind_listener() -> (Listener, SocketAddr) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let listener = Listener::new_identity_dispatch(socket, NonZeroUsize::new(8).unwrap());
    (listener, addr)
}

async fn dialer() -> tokio::net::UdpSocket {
    tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap()
}

fn key_of(conn: &Conn) -> SocketAddr {
    *conn.conn_key()
}

/// Park the accept future in its datagram read, drop it (a `select!` tick),
/// then dial one datagram that opens a flow and accept it.
async fn cancelled_at_the_read_then_dial() {
    let (listener, addr) = bind_listener().await;
    let dial = dialer().await;

    let mut parked = Box::pin(listener.poll_next_conn());
    let parked_at_read = poll_once(parked.as_mut()).is_pending();
    drop(parked);

    let dialer_addr = dial.local_addr().unwrap();
    dial.send_to(b"probe-a", addr).await.unwrap();

    let mut fresh = Box::pin(listener.poll_next_conn());
    match poll_briefly(fresh.as_mut()).await {
        Poll::Ready(Ok(conn)) => {
            let accepted = key_of(&conn) == dialer_addr;
            println!(
                "CASE cancelled_at_read_then_dial parked_at_read={parked_at_read} \
                 outcome=accepted flow_matches_dialer={accepted}"
            );
        }
        Poll::Ready(Err(error)) => {
            println!("CASE cancelled_at_read_then_dial outcome=error detail={error}");
        }
        Poll::Pending => {
            println!(
                "CASE cancelled_at_read_then_dial outcome=stranded \
                 detail=one datagram buffered, {POLL_BUDGET} polls, no accept"
            );
        }
    }
}

/// Park the combined accept loop in its datagram read, let a *second*
/// dispatcher consume a dialled datagram and enqueue the flow, then poll the
/// parked loop again with no further datagram available.
async fn concurrent_dispatcher_handover() {
    let (listener, addr) = bind_listener().await;
    let dial = dialer().await;

    let mut accept = Box::pin(listener.poll_next_conn());
    let parked_at_read = poll_once(accept.as_mut()).is_pending();

    let dialer_addr = dial.local_addr().unwrap();
    dial.send_to(b"probe-b", addr).await.unwrap();
    let dispatched = listener.dispatch_next().await.unwrap();

    match poll_briefly(accept.as_mut()).await {
        Poll::Ready(Ok(conn)) => {
            let accepted = key_of(&conn) == dialer_addr;
            println!(
                "CASE combined_concurrent_dispatcher parked_at_read={parked_at_read} \
                 dispatched={dispatched:?} handover=prompt flow_matches_dialer={accepted}"
            );
        }
        Poll::Ready(Err(error)) => {
            println!("CASE combined_concurrent_dispatcher outcome=error detail={error}");
        }
        Poll::Pending => {
            println!(
                "CASE combined_concurrent_dispatcher parked_at_read={parked_at_read} \
                 dispatched={dispatched:?} handover=deferred \
                 detail=flow queued by the other dispatcher, no further datagram"
            );
        }
    }
}

/// Same staging as [`concurrent_dispatcher_handover`], but one further
/// datagram from a second source arrives: the loop head drains the queued flow
/// before reading it, so the deferred handover completes.
async fn concurrent_dispatcher_then_further_datagram() {
    let (listener, addr) = bind_listener().await;
    let first = dialer().await;
    let second = dialer().await;

    let mut accept = Box::pin(listener.poll_next_conn());
    let parked_at_read = poll_once(accept.as_mut()).is_pending();

    let first_addr = first.local_addr().unwrap();
    first.send_to(b"probe-c-1", addr).await.unwrap();
    let _ = listener.dispatch_next().await.unwrap();
    let deferred = poll_briefly(accept.as_mut()).await.is_pending();

    second.send_to(b"probe-c-2", addr).await.unwrap();
    match poll_briefly(accept.as_mut()).await {
        Poll::Ready(Ok(conn)) => {
            let accepted = key_of(&conn) == first_addr;
            println!(
                "CASE combined_concurrent_dispatcher_then_datagram parked_at_read={parked_at_read} \
                 deferred_without_datagram={deferred} handover=ready flow_matches_first_dialer={accepted}"
            );
        }
        Poll::Ready(Err(error)) => {
            println!(
                "CASE combined_concurrent_dispatcher_then_datagram outcome=error detail={error}"
            );
        }
        Poll::Pending => {
            println!(
                "CASE combined_concurrent_dispatcher_then_datagram \
                 deferred_without_datagram={deferred} handover=deferred \
                 detail=still queued after a further datagram"
            );
        }
    }
}

/// The split form `common::udp_runtime` uses: one dispatcher, one acceptor that
/// only dequeues. Its wait is on the accept queue, so the flow is handed over.
async fn split_concurrent_dispatcher_handover() {
    let (listener, addr) = bind_listener().await;
    let dial = dialer().await;

    let mut accept = Box::pin(listener.accept_next());
    let parked_at_read = poll_once(accept.as_mut()).is_pending();

    let dialer_addr = dial.local_addr().unwrap();
    dial.send_to(b"probe-d", addr).await.unwrap();
    let dispatched = listener.dispatch_next().await.unwrap();

    match poll_briefly(accept.as_mut()).await {
        Poll::Ready(Some(conn)) => {
            let accepted = key_of(&conn) == dialer_addr;
            println!(
                "CASE split_concurrent_dispatcher parked_at_read={parked_at_read} \
                 dispatched={dispatched:?} handover=prompt flow_matches_dialer={accepted}"
            );
        }
        Poll::Ready(None) => {
            println!("CASE split_concurrent_dispatcher outcome=none_while_listener_alive");
        }
        Poll::Pending => {
            println!(
                "CASE split_concurrent_dispatcher parked_at_read={parked_at_read} \
                 dispatched={dispatched:?} handover=deferred"
            );
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    cancelled_at_the_read_then_dial().await;
    concurrent_dispatcher_handover().await;
    concurrent_dispatcher_then_further_datagram().await;
    split_concurrent_dispatcher_handover().await;
    println!(
        "PROBE_SUMMARY single_driver_cancellation_and_split_form_are_the_only_prompt_cases; \
         combined_concurrent_handover_is_deferred_until_a_pin_passes_\
         `wake the combined accept loop on a queued flow`"
    );
}
