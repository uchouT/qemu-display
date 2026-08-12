//! usbredir data packets awaiting their RDPEUSB completion.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Context as _;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use ironrdp_server::{CompletionFut, PendingHandle, PendingRequest, UsbRequestCompletion};

/// Asynchronous usbredir data packets that still owe QEMU a response.
///
/// usbredir matches responses to requests by ID: every data packet the guest
/// submits must be answered exactly once, or its URB never completes. An entry
/// in `pending_ctx` records that outstanding debt, so the two collections stay
/// in one-to-one correspondence:
///
/// - a submission inserts into both,
/// - a cancellation touches neither,
/// - [`DataInFlight::completed`] is the only place that removes from either.
///
/// Request state is split by the phase it applies to: `pending_ctx` holds what
/// is only usable while the request is unfinished, and [`ResponseContext`]
/// travels with the future because it is inert until the completion arrives.
///
/// Dropping the collection abandons every outstanding request: each wrapped
/// [`CompletionFut`] requests cancellation while the device channel is open.
pub(super) struct DataInFlight {
    futures: FuturesUnordered<DataArrivedFut>,
    pending_ctx: HashMap<u64, PendingContext>,
}

/// Carries a request's response context alongside its completion future.
///
/// [`CompletionFut`] does not identify its own request, and the context has to
/// stay available on the failure path too: a request that ends without a
/// completion still owes QEMU a response. Riding with the future also makes it
/// impossible to pair a completion with another request's context.
struct DataArrivedFut {
    inner: CompletionFut,
    /// Taken when the request ends, so the future yields at most once.
    ctx: Option<ResponseContext>,
}

/// Request state that is only usable while the request is unfinished.
struct PendingContext {
    /// Requests cancellation without consuming the entry: a cancelled packet
    /// is still answered by its normal completion.
    handle: PendingHandle,
}

/// usbredir state that an RDPEUSB completion does not carry.
pub(super) struct ResponseContext {
    pub(super) usbredir_id: u64,
}

/// handler can use this data to dispatch their handle logic.
pub(super) struct DataCompleted {
    pub(super) ctx: ResponseContext,
    pub(super) result: anyhow::Result<UsbRequestCompletion>,
}

impl Future for DataArrivedFut {
    type Output = (ResponseContext, anyhow::Result<UsbRequestCompletion>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `CompletionFut` is `Unpin`, so the inner future needs no projection.
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll(cx).map(|result| {
            // `FuturesUnordered` drops a future as soon as it is ready, and
            // nothing else polls one, so the context is always still here.
            let ctx = this
                .ctx
                .take()
                .expect("usbredir data future polled after completion");
            (ctx, result)
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

    pub(super) fn push_bulk(&mut self, pending: PendingRequest /* other args */) {}

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
    pub(super) async fn completed(&mut self) -> anyhow::Result<DataCompleted> {
        loop {
            let arrived = self.futures.next().await;
            match arrived {
                Some((ctx, result)) => return self.release(ctx, result),
                // An empty `FuturesUnordered` resolves immediately. Hold the
                // branch instead of spinning the session loop.
                None => std::future::pending::<()>().await,
            }
        }
    }

    /// Cancel-safety: [`FuturesUnordered`] only removes a future once it
    /// yields, so this must stay synchronous. An await between the yield and
    /// the response would let [`DataInFlight::poll`] be dropped after the
    /// completion has left the collection, losing a response QEMU is waiting
    /// for.
    fn release(
        &mut self,
        ctx: ResponseContext,
        result: anyhow::Result<UsbRequestCompletion>,
    ) -> anyhow::Result<DataCompleted> {
        let usbredir_id = ctx.usbredir_id;
        self.pending_ctx.remove(&usbredir_id).with_context(|| {
            format!("usbredir data packet {usbredir_id} completed without a pending context")
        })?;

        Ok(DataCompleted { ctx, result })
    }
}
