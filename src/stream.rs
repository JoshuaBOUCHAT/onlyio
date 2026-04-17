use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Équivalent local de `futures::Stream` — pas de dépendance externe.
///
/// `poll_next` suit la même sémantique que `Future::poll` :
/// - `Poll::Pending`       → pas encore de valeur disponible
/// - `Poll::Ready(Some(v))` → valeur disponible
/// - `Poll::Ready(None)`  → stream terminé, ne plus poller
pub trait Stream {
    type Item;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>;
}

/// Extension automatique sur tout `Stream : Unpin` pour obtenir `.next().await`.
pub trait StreamExt: Stream {
    fn next(&mut self) -> Next<'_, Self>
    where
        Self: Unpin,
    {
        Next(self)
    }
}

impl<S: Stream> StreamExt for S {}

/// Future produite par `StreamExt::next()`.
pub struct Next<'a, S: Stream + ?Sized>(pub &'a mut S);

impl<S: Stream + Unpin> Future for Next<'_, S> {
    type Output = Option<S::Item>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        Pin::new(&mut *self.0).poll_next(cx)
    }
}
