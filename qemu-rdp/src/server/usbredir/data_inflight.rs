//! RDPEUSB requests awaiting their completion.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Context as _;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use ironrdp_server::{CompletionFut, PendingHandle, PendingRequest, UsbRequestCompletion};

use super::transfer::ResponseContext;

/// RDPEUSB requests that have been submitted and have not ended yet.
///
/// The collection tracks request lifetime; what a request is *for* rides with
/// it as a [`Purpose`] and is never interpreted here. Only the per-purpose
/// index needed to reach a request while it is unfinished lives alongside the
/// futures, and [`DataInFlight::completed`] is the only place that removes from
/// one.
///
/// `pending_ctx` is such an index: usbredir matches responses to requests by
/// ID, so a data packet's entry is what a `CancelDataPacket` naming that ID
/// looks up. Submission inserts, cancellation touches nothing, completion
/// removes.
///
/// Dropping the collection abandons every outstanding request: each wrapped
/// [`CompletionFut`] requests cancellation while the device channel is open.
pub(super) struct DataInFlight {
    futures: FuturesUnordered<DataArrivedFut>,
    pending_ctx: HashMap<u64, PendingContext>,
}

/// Carries a request's purpose alongside its completion future.
///
/// [`CompletionFut`] does not identify its own request, and the purpose has to
/// stay available on the failure path too: a request that ends without a
/// completion may still owe QEMU a response. Riding with the future also makes
/// it impossible to pair a completion with another request's purpose.
struct DataArrivedFut {
    inner: CompletionFut,
    /// Taken when the request ends, so the future yields at most once.
    purpose: Option<Purpose>,
}

/// Request state that is only usable while the request is unfinished.
struct PendingContext {
    /// Requests cancellation without consuming the entry: a cancelled packet
    /// is still answered by its normal completion.
    handle: PendingHandle,
}

/// Why a request was submitted, and therefore how its completion is handled.
///
/// Each payload belongs to the module that gives it meaning; this enum only
/// names the kinds, and reads no more of a payload than the index for that kind
/// needs.
pub(super) enum Purpose {
    /// A usbredir data packet whose ID still owes QEMU a response.
    Data(ResponseContext),
}

/// A request that ended, with everything needed to dispatch it.
pub(super) struct Completed {
    pub(super) purpose: Purpose,
    pub(super) result: anyhow::Result<UsbRequestCompletion>,
}

impl Future for DataArrivedFut {
    type Output = (Purpose, anyhow::Result<UsbRequestCompletion>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `CompletionFut` is `Unpin`, so the inner future needs no projection.
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll(cx).map(|result| {
            // `FuturesUnordered` drops a future as soon as it is ready, and
            // nothing else polls one, so the purpose is always still here.
            let purpose = this
                .purpose
                .take()
                .expect("usbredir request future polled after completion");
            (purpose, result)
        })
    }
}

impl DataInFlight {
    pub(super) fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
            pending_ctx: HashMap::new(),
        }
    }

    /// Tracks a submitted request until it ends.
    ///
    /// The match selects the index a purpose is reachable through; it never
    /// decides what the request does. Indexing runs before the future is
    /// stored, so a rejected request leaves nothing behind: a reused packet ID
    /// is a peer protocol violation, and overwriting the entry would silently
    /// drop the response the first one is owed.
    pub(super) fn push(&mut self, pending: PendingRequest, purpose: Purpose) -> anyhow::Result<()> {
        match &purpose {
            Purpose::Data(ctx) => {
                let usbredir_id = ctx.usbredir_id;
                anyhow::ensure!(
                    !self.pending_ctx.contains_key(&usbredir_id),
                    "usbredir data packet {usbredir_id} is already in flight"
                );

                let handle = pending.pending_handle();
                self.pending_ctx
                    .insert(usbredir_id, PendingContext { handle });
            }
        }

        self.futures.push(DataArrivedFut {
            inner: pending.wait(),
            purpose: Some(purpose),
        });
        Ok(())
    }

    /// Requests cancellation of a pending usbredir data packet.
    ///
    /// Cancellation is advisory: the request stays in flight and its completion
    /// still produces the response, which may report success. Cancelling an
    /// unknown ID is a no-op, as usbredir requires.
    pub(super) fn cancel(&self, usbredir_id: u64) {
        let Some(ctx) = self.pending_ctx.get(&usbredir_id) else {
            tracing::trace!(
                usbredir_id,
                "Ignored cancellation for an unknown or completed usbredir packet"
            );
            return;
        };

        ctx.handle.cancel();
        tracing::trace!(usbredir_id, "Requested RDPEUSB transfer cancellation");
    }

    /// Waits for the next request to end and releases its pending state.
    pub(super) async fn completed(&mut self) -> anyhow::Result<Completed> {
        loop {
            let arrived = self.futures.next().await;
            match arrived {
                Some((purpose, result)) => return self.release(purpose, result),
                // An empty `FuturesUnordered` resolves immediately. Hold the
                // branch instead of spinning the session loop.
                None => std::future::pending::<()>().await,
            }
        }
    }

    /// Cancel-safety: [`FuturesUnordered`] only removes a future once it
    /// yields, so this must stay synchronous. An await between the yield and
    /// the response would let [`DataInFlight::completed`] be dropped after the
    /// completion has left the collection, losing a response QEMU is waiting
    /// for.
    fn release(
        &mut self,
        purpose: Purpose,
        result: anyhow::Result<UsbRequestCompletion>,
    ) -> anyhow::Result<Completed> {
        match &purpose {
            Purpose::Data(ctx) => {
                let usbredir_id = ctx.usbredir_id;
                self.pending_ctx.remove(&usbredir_id).with_context(|| {
                    format!(
                        "usbredir data packet {usbredir_id} completed without a pending context"
                    )
                })?;
            }
        }

        Ok(Completed { purpose, result })
    }
}
