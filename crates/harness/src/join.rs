//! Pure-`std` concurrent `join_all` over boxed futures.

use hrn_kernel::ScopedFuture;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Pure-`std` concurrent `join_all` polling a vector of boxed `Send` futures to completion.
pub struct JoinAll<'a, T> {
    futs: Vec<Option<ScopedFuture<'a, T>>>,
    results: Vec<Option<T>>,
}

impl<T: Unpin> Future for JoinAll<'_, T> {
    type Output = Vec<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut pending = false;
        for (slot, result) in this.futs.iter_mut().zip(this.results.iter_mut()) {
            if let Some(fut) = slot.as_mut() {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(val) => {
                        *result = Some(val);
                        *slot = None;
                    }
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(this.results.iter_mut().filter_map(Option::take).collect())
        }
    }
}

/// Concurrently await all boxed futures and preserve input order.
pub fn join_all<T: Unpin>(futs: Vec<ScopedFuture<'_, T>>) -> JoinAll<'_, T> {
    let results = futs.iter().map(|_| None).collect();
    JoinAll {
        futs: futs.into_iter().map(Some).collect(),
        results,
    }
}
