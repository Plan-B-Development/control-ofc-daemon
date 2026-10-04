//! Lending the OpenFan poll loop's serial port to a firmware update (DEC-481).
//!
//! The poll loop is the only code that swaps what the port slot holds — at a
//! reconnect, and here. An update asks for the port through a channel the loop
//! listens on between polls; the loop puts [`MaintenanceTransport`] in the slot,
//! hands the real port over, and then waits for it to come back. While the port
//! is out the loop sends no polls, counts no failures, and neither releases,
//! searches for nor opens anything: it is parked in [`lend`].
//!
//! Both sides of the hand-back are fixed here:
//! - **a port comes back** — the loop installs it, re-seeds its reconnect search
//!   for the node it arrived on, resets its counters and reports `Connected`;
//! - **nothing comes back** (the board is gone, or the update ended without
//!   giving it back, panics included) — the slot gets [`DisconnectedTransport`]
//!   and the loop starts its normal reconnect search.
//!
//! The update learns that the loop has acted on the hand-back through
//! [`LoanHandle::give_back`], so it lifts the write suspension only once the
//! port is really back in the slot.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::serial::transport::{DisconnectedTransport, MaintenanceTransport, SerialTransport};

/// The slot the poll loop and `FanController` share.
pub type PortSlot = Arc<parking_lot::Mutex<Box<dyn SerialTransport + Send>>>;

/// The sending half, kept by `AppState` for the update to borrow through.
pub type LoanSender = mpsc::Sender<PortLoanRequest>;

/// The receiving half, owned by the poll loop.
pub type LoanReceiver = mpsc::Receiver<PortLoanRequest>;

/// A lending channel. Capacity 1: there is only ever one update at a time.
pub fn loan_channel() -> (LoanSender, LoanReceiver) {
    mpsc::channel(1)
}

/// A firmware update's request for the port.
pub struct PortLoanRequest {
    reply: oneshot::Sender<Result<PortLoan, LoanRefusal>>,
    /// Lend even while the loop's last poll failed (DEC-484): an update of a
    /// board that does not answer takes the slot whatever it holds.
    accept_disconnected: bool,
}

/// The port, on loan. `transport` belongs to the borrower until it gives a
/// port (this one, or one it opened after the board came back) or nothing back
/// through `handle`.
pub struct PortLoan {
    pub transport: Box<dyn SerialTransport + Send>,
    pub handle: LoanHandle,
}

/// The borrower's side of the hand-back. Dropping it without calling
/// [`Self::give_back`] — a panicking update — counts as giving nothing back.
pub struct LoanHandle {
    give_back: oneshot::Sender<LoanReturn>,
    settled: oneshot::Receiver<()>,
}

/// What an update hands back.
pub enum LoanReturn {
    /// A port the poll loop can use, and the node it is open on.
    Port {
        transport: Box<dyn SerialTransport + Send>,
        path: String,
    },
    /// Nothing usable: the loop searches for the controller as after a loss.
    Nothing,
}

/// Why the port was not lent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoanRefusal {
    /// The loop's last poll failed: it is retrying, or has given up on the port
    /// and is searching for the controller.
    NotConnected,
    /// No poll loop is listening: none was started, or it has stopped.
    NoPollLoop,
    /// The loop did not answer in time.
    Timeout,
}

impl LoanRefusal {
    pub fn describe(self) -> &'static str {
        match self {
            Self::NotConnected => "the OpenFan connection is not answering",
            Self::NoPollLoop => "no OpenFan poll loop is running",
            Self::Timeout => "the OpenFan poll loop did not answer in time",
        }
    }
}

/// Ask the poll loop for its port, waiting at most `wait` for each of the two
/// steps (the request reaching the loop, and its answer). Refused unless the
/// loop's last poll succeeded.
///
/// The loop handles a request between polls, so `wait` should cover one poll
/// at the serial timeout. A wait that runs out loses no port: an answer the
/// loop sent before the borrower gave up is taken, and one it had not sent yet
/// fails to send, so the loop puts the port straight back.
pub async fn borrow(lender: &LoanSender, wait: Duration) -> Result<PortLoan, LoanRefusal> {
    request(lender, wait, false).await
}

/// As [`borrow`], whatever the loop's link (DEC-484): the update of a board
/// that does not answer parks the loop so its reconnect search stops opening
/// the board. The transport lent may be [`DisconnectedTransport`] — a
/// placeholder ([`SerialTransport::is_placeholder`]) — or a port whose board
/// has stopped answering.
pub async fn borrow_any(lender: &LoanSender, wait: Duration) -> Result<PortLoan, LoanRefusal> {
    request(lender, wait, true).await
}

async fn request(
    lender: &LoanSender,
    wait: Duration,
    accept_disconnected: bool,
) -> Result<PortLoan, LoanRefusal> {
    let (reply, mut answer) = oneshot::channel();
    let ask = PortLoanRequest {
        reply,
        accept_disconnected,
    };
    match tokio::time::timeout(wait, lender.send(ask)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err(LoanRefusal::NoPollLoop),
        Err(_) => return Err(LoanRefusal::Timeout),
    }
    match tokio::time::timeout(wait, &mut answer).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(LoanRefusal::NoPollLoop),
        Err(_) => late_answer(answer),
    }
}

/// The borrower's wait ran out. Close the channel first, so a loop that has
/// not sent yet fails to and puts the port back; then take an answer sent
/// before the close. Dropping the receiver unread instead would drop a loan
/// that landed as the wait ended — the port with it, closed without
/// discarding its output, and the loop left to reconnect from nothing.
fn late_answer(
    mut answer: oneshot::Receiver<Result<PortLoan, LoanRefusal>>,
) -> Result<PortLoan, LoanRefusal> {
    answer.close();
    answer.try_recv().unwrap_or(Err(LoanRefusal::Timeout))
}

impl LoanHandle {
    /// Hand `ret` back and wait, at most `wait`, until the poll loop has acted
    /// on it. `false` if the loop is gone (shutting down) or did not settle in
    /// time — in which case the caller must not assume the port is installed.
    pub async fn give_back(self, ret: LoanReturn, wait: Duration) -> bool {
        if self.give_back.send(ret).is_err() {
            return false;
        }
        matches!(tokio::time::timeout(wait, self.settled).await, Ok(Ok(())))
    }
}

/// What became of a request, for the poll loop to act on.
pub enum LoanEnd {
    /// The stop signal arrived while the port was out; the loop must return.
    Shutdown,
    /// Not lent: the loop's last poll failed.
    Refused,
    /// The borrower stopped waiting before it took the port; it is back in the
    /// slot and nothing changed.
    Abandoned,
    /// A port came back and is installed. The loop re-seeds its search for
    /// `path`, resets its counters, then signals `settled`.
    Returned {
        path: String,
        settled: oneshot::Sender<()>,
    },
    /// Nothing came back; the slot holds [`DisconnectedTransport`]. The loop
    /// enters its reconnect search, then signals `settled`.
    Lost { settled: oneshot::Sender<()> },
}

/// The poll loop's half: lend the port in `slot` to `request`'s borrower and
/// wait for the hand-back, or refuse unless the loop is `connected` — its last
/// poll succeeded, so it holds a port that answers — or the request accepts a
/// loop that is not ([`borrow_any`]).
///
/// Only the poll loop calls this, and only between polls, so no poll or
/// reconnect attempt of its own can be in flight. The swap runs on the blocking
/// pool because the engine may hold the slot across a serial write; the
/// installs at the end only replace the placeholder, which nothing holds for
/// longer than a failed call.
pub async fn lend(
    slot: &PortSlot,
    request: PortLoanRequest,
    connected: bool,
    shutdown: &mut watch::Receiver<bool>,
) -> LoanEnd {
    if !connected && !request.accept_disconnected {
        let _ = request.reply.send(Err(LoanRefusal::NotConnected));
        return LoanEnd::Refused;
    }
    let taken = slot.clone();
    let port = match tokio::task::spawn_blocking(move || {
        std::mem::replace(
            &mut *taken.lock(),
            Box::new(MaintenanceTransport) as Box<dyn SerialTransport + Send>,
        )
    })
    .await
    {
        Ok(port) => port,
        Err(e) => {
            // A panicked swap leaves the slot as it was: the closure has no
            // other side effect.
            log::error!("OpenFan poll loop: could not take the port to lend it: {e}");
            let _ = request.reply.send(Err(LoanRefusal::NoPollLoop));
            return LoanEnd::Refused;
        }
    };
    let (give_back, returned) = oneshot::channel();
    let (settled_tx, settled) = oneshot::channel();
    let loan = PortLoan {
        transport: port,
        handle: LoanHandle { give_back, settled },
    };
    if let Err(Ok(loan)) = request.reply.send(Ok(loan)) {
        *slot.lock() = loan.transport;
        return LoanEnd::Abandoned;
    }
    log::info!("OpenFan poll loop: port lent to a firmware update — polling paused");
    let returned = tokio::select! {
        // Shutdown first, as in every poll-loop wait (DEC-272).
        biased;
        _ = shutdown.changed() => return LoanEnd::Shutdown,
        r = returned => r.unwrap_or(LoanReturn::Nothing),
    };
    match returned {
        LoanReturn::Port { transport, path } => {
            *slot.lock() = transport;
            log::info!("OpenFan poll loop: port {path} handed back — polling resumes");
            LoanEnd::Returned {
                path,
                settled: settled_tx,
            }
        }
        LoanReturn::Nothing => {
            *slot.lock() = Box::new(DisconnectedTransport);
            log::warn!(
                "OpenFan poll loop: the firmware update gave no port back — searching for the \
                 controller"
            );
            LoanEnd::Lost {
                settled: settled_tx,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SerialError;

    struct Named(&'static str);

    impl SerialTransport for Named {
        fn write_line(&mut self, _data: &str) -> Result<(), SerialError> {
            Ok(())
        }
        fn read_line(&mut self, _timeout: Duration) -> Result<String, SerialError> {
            Err(SerialError::Protocol {
                message: self.0.to_string(),
            })
        }
    }

    fn name_of(t: &mut dyn SerialTransport) -> String {
        match t.read_line(Duration::ZERO) {
            Err(SerialError::Protocol { message }) => message,
            other => format!("{other:?}"),
        }
    }

    fn slot_with(name: &'static str) -> PortSlot {
        Arc::new(parking_lot::Mutex::new(Box::new(Named(name))))
    }

    #[tokio::test]
    async fn a_refusal_while_not_connected_leaves_the_slot_alone() {
        let slot = slot_with("real");
        let (lender, mut loans) = loan_channel();
        let (_stop, mut shutdown) = watch::channel(false);
        let asked = tokio::spawn(async move { borrow(&lender, Duration::from_secs(5)).await });
        let request = loans.recv().await.expect("a request");
        assert!(matches!(
            lend(&slot, request, false, &mut shutdown).await,
            LoanEnd::Refused
        ));
        assert_eq!(
            asked.await.unwrap().err(),
            Some(LoanRefusal::NotConnected),
            "the borrower must hear why"
        );
        assert_eq!(name_of(&mut **slot.lock()), "real");
    }

    /// DEC-484: an update of a board that does not answer borrows whatever
    /// the slot holds — the loop is parked all the same, and what comes back
    /// is installed as from any other loan.
    #[tokio::test]
    async fn a_silent_boards_update_borrows_from_a_loop_that_is_not_connected() {
        let slot = slot_with("reconnecting placeholder");
        let (lender, mut loans) = loan_channel();
        let (_stop, mut shutdown) = watch::channel(false);
        let borrower = tokio::spawn(async move {
            let mut loan = borrow_any(&lender, Duration::from_secs(5)).await.unwrap();
            assert_eq!(name_of(&mut *loan.transport), "reconnecting placeholder");
            loan.handle
                .give_back(
                    LoanReturn::Port {
                        transport: Box::new(Named("answering")),
                        path: "/dev/ttyACM9".into(),
                    },
                    Duration::from_secs(5),
                )
                .await
        });
        let request = loans.recv().await.expect("a request");
        let LoanEnd::Returned { settled, .. } = lend(&slot, request, false, &mut shutdown).await
        else {
            panic!("lent while not connected, and the port came back");
        };
        assert_eq!(name_of(&mut **slot.lock()), "answering");
        settled.send(()).unwrap();
        assert!(borrower.await.unwrap());
    }

    #[tokio::test]
    async fn a_lent_port_leaves_the_placeholder_until_it_is_handed_back() {
        let slot = slot_with("real");
        let (lender, mut loans) = loan_channel();
        let (_stop, mut shutdown) = watch::channel(false);
        let borrower_slot = slot.clone();
        let borrower = tokio::spawn(async move {
            let mut loan = borrow(&lender, Duration::from_secs(5)).await.unwrap();
            assert_eq!(
                name_of(&mut *loan.transport),
                "real",
                "the real port is lent"
            );
            // While it is out, the slot holds the placeholder.
            let held = name_of(&mut **borrower_slot.lock());
            assert!(held.contains("firmware update"), "slot held {held:?}");
            loan.handle
                .give_back(
                    LoanReturn::Port {
                        transport: Box::new(Named("new")),
                        path: "/dev/ttyACM9".into(),
                    },
                    Duration::from_secs(5),
                )
                .await
        });
        let request = loans.recv().await.expect("a request");
        let LoanEnd::Returned { path, settled } = lend(&slot, request, true, &mut shutdown).await
        else {
            panic!("the port came back");
        };
        assert_eq!(path, "/dev/ttyACM9");
        assert_eq!(
            name_of(&mut **slot.lock()),
            "new",
            "the returned port is installed"
        );
        settled.send(()).unwrap();
        assert!(
            borrower.await.unwrap(),
            "the borrower hears the loop settled"
        );
    }

    #[tokio::test]
    async fn a_borrower_that_drops_its_handle_counts_as_giving_nothing_back() {
        let slot = slot_with("real");
        let (lender, mut loans) = loan_channel();
        let (_stop, mut shutdown) = watch::channel(false);
        let borrower = tokio::spawn(async move {
            let loan = borrow(&lender, Duration::from_secs(5)).await.unwrap();
            drop(loan);
        });
        let request = loans.recv().await.expect("a request");
        assert!(matches!(
            lend(&slot, request, true, &mut shutdown).await,
            LoanEnd::Lost { .. }
        ));
        borrower.await.unwrap();
        let held = name_of(&mut **slot.lock());
        assert!(held.contains("reconnecting"), "slot held {held:?}");
    }

    #[tokio::test]
    async fn shutdown_while_the_port_is_out_ends_the_wait() {
        let slot = slot_with("real");
        let (lender, mut loans) = loan_channel();
        let (stop, mut shutdown) = watch::channel(false);
        let borrower = tokio::spawn(async move {
            let loan = borrow(&lender, Duration::from_secs(5)).await.unwrap();
            // Hold the port until the test ends.
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(loan);
        });
        let request = loans.recv().await.expect("a request");
        let lending = tokio::spawn(async move {
            let end = lend(&slot, request, true, &mut shutdown).await;
            matches!(end, LoanEnd::Shutdown)
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop.send(true).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), lending)
            .await
            .expect("the wait ends on shutdown")
            .unwrap());
        borrower.abort();
    }

    #[tokio::test]
    async fn a_borrower_that_gave_up_gets_its_port_put_straight_back() {
        let slot = slot_with("real");
        let (lender, mut loans) = loan_channel();
        let (_stop, mut shutdown) = watch::channel(false);
        // The request reaches the loop, then the borrower stops waiting.
        let (reply, answer) = oneshot::channel();
        lender
            .send(PortLoanRequest {
                reply,
                accept_disconnected: false,
            })
            .await
            .unwrap();
        drop(answer);
        let request = loans.recv().await.expect("a request");
        assert!(matches!(
            lend(&slot, request, true, &mut shutdown).await,
            LoanEnd::Abandoned
        ));
        assert_eq!(name_of(&mut **slot.lock()), "real");
    }

    /// The loop's answer lands just as the borrower's wait runs out (on the
    /// multi-threaded runtime, between the timeout's last look and its
    /// return). Dropped unread, the loan took the port with it and the loop
    /// heard "nothing back"; taken, the port comes home.
    #[tokio::test]
    async fn a_loan_that_lands_as_the_wait_ends_is_taken_not_dropped() {
        let slot = slot_with("real");
        let (lender, mut loans) = loan_channel();
        let (_stop, mut shutdown) = watch::channel(false);
        let (reply, answer) = oneshot::channel();
        lender
            .send(PortLoanRequest {
                reply,
                accept_disconnected: false,
            })
            .await
            .unwrap();
        let request = loans.recv().await.expect("a request");
        let lending = {
            let slot = slot.clone();
            tokio::spawn(async move { lend(&slot, request, true, &mut shutdown).await })
        };
        // The loop has answered; the borrower has not read it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while answer.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the loop never answered"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // The wait runs out now.
        let mut loan = late_answer(answer).expect("the loan that landed is taken");
        assert_eq!(name_of(&mut *loan.transport), "real");
        let back = tokio::spawn(loan.handle.give_back(
            LoanReturn::Port {
                transport: loan.transport,
                path: "/dev/ttyACM0".into(),
            },
            Duration::from_secs(5),
        ));
        match lending.await.unwrap() {
            LoanEnd::Returned { settled, .. } => {
                let _ = settled.send(());
            }
            _ => panic!("the port must come back to the loop"),
        }
        assert!(back.await.unwrap());
        assert_eq!(name_of(&mut **slot.lock()), "real");
    }

    /// A wait that ran out before the loop answered closes the channel, so the
    /// loop's answer fails to send and it puts the port straight back.
    #[tokio::test]
    async fn a_wait_that_ran_out_first_turns_the_loan_away() {
        let slot = slot_with("real");
        let (lender, mut loans) = loan_channel();
        let (_stop, mut shutdown) = watch::channel(false);
        let (reply, answer) = oneshot::channel();
        lender
            .send(PortLoanRequest {
                reply,
                accept_disconnected: false,
            })
            .await
            .unwrap();
        assert!(matches!(late_answer(answer), Err(LoanRefusal::Timeout)));
        let request = loans.recv().await.expect("a request");
        assert!(matches!(
            lend(&slot, request, true, &mut shutdown).await,
            LoanEnd::Abandoned
        ));
        assert_eq!(name_of(&mut **slot.lock()), "real");
    }
}
