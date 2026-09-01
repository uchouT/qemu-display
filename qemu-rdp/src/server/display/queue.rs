#![allow(dead_code)]
// I wish I could reuse tokio::mpsc instead of creating a new queue from scratch
use ironrdp::{
    core::assert_impl,
    server::{BitmapUpdate, DisplayUpdate},
};
use ironrdp_server::DisplayUpdate::LargePointer;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::{Mutex, MutexGuard, Notify};

use DisplayUpdate::{
    Bitmap, CachedPointer, ColorPointer, DefaultPointer, HidePointer, RGBAPointer, Resize,
};

mod rect;
use rect::Rect;

// The queue capacity is not fixed, as its size may vary depending on how pending updates are splitted
// however, if there are more than 30 updates pending, the producer will be blocked until the consumer.
const QUEUE_CAPACITY: usize = 30;

pub(crate) struct DisplayQueue {
    queue: Mutex<VecDeque<DisplayUpdate>>,
    notify_producer: Notify,
    notify_consumer: Notify,
    is_closed: AtomicBool,
    senders: AtomicUsize,
}

pub(crate) struct Sender {
    channel: Arc<DisplayQueue>,
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.channel.senders.fetch_add(1, Ordering::SeqCst);
        Self {
            channel: Arc::clone(&self.channel),
        }
    }
}
assert_impl!(Sender: Send);

impl Drop for Sender {
    fn drop(&mut self) {
        if self.channel.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.channel.close();
        }
    }
}

pub(crate) struct Receiver {
    channel: Arc<DisplayQueue>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.channel.close();
    }
}

impl DisplayQueue {
    pub(crate) fn channel() -> (Sender, Receiver) {
        let channel = Arc::new(DisplayQueue {
            queue: Mutex::new(VecDeque::with_capacity(QUEUE_CAPACITY)),
            notify_producer: Notify::new(),
            notify_consumer: Notify::new(),
            is_closed: AtomicBool::new(false),
            senders: AtomicUsize::new(1),
        });
        (
            Sender {
                channel: Arc::clone(&channel),
            },
            Receiver { channel },
        )
    }

    fn close(&self) {
        self.is_closed.store(true, Ordering::SeqCst);
        self.notify_consumer.notify_one();
        self.notify_producer.notify_one();
    }

    fn closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }
}

impl Sender {
    pub(crate) async fn send(&self, item: DisplayUpdate) -> Result<(), ()> {
        self.push_update(item, true).await
    }

    async fn push_update(&self, item: DisplayUpdate, update: bool) -> Result<(), ()> {
        loop {
            if self.channel.closed() {
                return Err(());
            }

            let mut queue = self.channel.queue.lock().await;
            if queue.len() < QUEUE_CAPACITY {
                if update {
                    if push_update(&mut queue, item) {
                        self.channel.notify_consumer.notify_one();
                    }
                } else {
                    queue.push_back(item);
                    self.channel.notify_consumer.notify_one();
                }
                return Ok(());
            } else {
                drop(queue);
                self.channel.notify_producer.notified().await;
            }
        }
    }
}

fn push_update(queue: &mut MutexGuard<VecDeque<DisplayUpdate>>, item: DisplayUpdate) -> bool {
    // Try to optimize the pending updates, while not being too aggressive for a reasonable experience
    match item {
        DisplayUpdate::PointerPosition { .. } => {
            // If there is already a pointer position update in the queue, replace it with the new one
            if let Some(idx) = queue
                .iter()
                .position(|update| matches!(update, DisplayUpdate::PointerPosition { .. }))
            {
                queue[idx] = item;
                return true;
            }
        }
        LargePointer(_) | ColorPointer(_) | RGBAPointer(_) | CachedPointer(_) | HidePointer | DefaultPointer => {
            // If there is already a pointer shape update in the queue, replace it with the new one
            if let Some(idx) = queue.iter().position(|update| {
                matches!(
                    update,
                    LargePointer(_) | ColorPointer(_) | RGBAPointer(_) | CachedPointer(_) | HidePointer | DefaultPointer
                )
            }) {
                queue[idx] = item;
                return true;
            }
        }
        Resize(_) => {
            // drop all graphics operations and keep only the last resize operation
            // Note: pointer updates are not considered graphics operations
            queue.retain(|item| !matches!(item, Resize(_) | Bitmap(_)));
        }
        Bitmap(ref update @ BitmapUpdate { .. }) => {
            // drop and split bitmap updates if they overlap
            let mut i = 0;
            while i < queue.len() {
                let prev = match &queue[i] {
                    Bitmap(prev) => prev,
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                if let Some(res) = mask_bitmap_update(update, prev) {
                    tracing::debug!(res = res.len(), "dropping");
                    queue.remove(i);
                    for item in res {
                        queue.insert(i, Bitmap(item));
                        i += 1;
                    }
                }
                i += 1;
            }
        }
    }
    queue.push_back(item);
    true
}

impl From<&BitmapUpdate> for Rect {
    fn from(update: &BitmapUpdate) -> Self {
        Rect {
            x: update.x as _,
            y: update.y as _,
            width: update.width.get() as _,
            height: update.height.get() as _,
        }
    }
}

fn mask_bitmap_update(mask: &BitmapUpdate, prev: &BitmapUpdate) -> Option<Vec<BitmapUpdate>> {
    let mask_rect = Rect::from(mask);
    let prev_rect = Rect::from(prev);
    if matches!(prev_rect.intersect(&mask_rect), Some(vec) if vec.is_empty()) {
        // full overlap discards the prev update
        return Some(vec![]);
    }
    // else TODO: split the updates.. sharing buffer is not supported yet
    None
}

impl Receiver {
    pub(crate) async fn recv(&self) -> Option<DisplayUpdate> {
        loop {
            let mut queue = self.channel.queue.lock().await;
            if let Some(item) = queue.pop_front() {
                self.channel.notify_producer.notify_one();
                return Some(item);
            } else if self.channel.closed() && queue.is_empty() {
                return None;
            } else {
                drop(queue);
                self.channel.notify_consumer.notified().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZero;

    use bytes::Bytes;
    use ironrdp_server::{DesktopSize, PixelFormat};

    use super::*;

    #[test]
    fn test_send_recv() {
        let (tx, rx) = DisplayQueue::channel();
        crate::utils::block_on(async move {
            let tx_task = tokio::spawn(async move {
                let ops = vec![
                    // dummy
                    Resize(DesktopSize {
                        width: 1024,
                        height: 768,
                    }),
                ];
                for op in ops {
                    tx.send(op).await.unwrap();
                }
            });
            let rx_task = tokio::spawn(async move {
                let _ = rx.recv().await;
            });
            let _ = tokio::join!(tx_task, rx_task);
        });
    }

    #[test]
    fn test_capacity() {
        let (tx, rx) = DisplayQueue::channel();
        crate::utils::block_on(async move {
            let ops = vec![DefaultPointer; QUEUE_CAPACITY];
            for op in ops {
                tx.push_update(op, false).await.unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_millis(200), async move {
                tx.push_update(DefaultPointer, false).await.unwrap();
            })
            .await
            .unwrap_err();
            for _ in 0..QUEUE_CAPACITY {
                let _ = rx.recv().await;
            }
        });
    }

    #[test]
    fn test_close() {
        let (tx, rx) = DisplayQueue::channel();
        crate::utils::block_on(async move {
            let tx_task = tokio::spawn(async move {
                drop(tx);
            });
            let rx_task = tokio::spawn(async move {
                let res = rx.recv().await;
                assert!(res.is_none());
            });
            let _ = tokio::join!(tx_task, rx_task);
        });
    }

    #[test]
    fn test_resize() {
        let (tx, rx) = DisplayQueue::channel();
        crate::utils::block_on(async move {
            let ops = vec![
                Resize(DesktopSize {
                    width: 1024,
                    height: 768,
                }),
                Bitmap(BitmapUpdate {
                    x: 0,
                    y: 0,
                    stride: NonZero::new(1usize).unwrap(),
                    width: NonZero::new(1024).unwrap(),
                    height: NonZero::new(768).unwrap(),
                    format: PixelFormat::ABgr32,
                    data: Bytes::new(),
                }),
                Resize(DesktopSize {
                    width: 800,
                    height: 600,
                }),
                DefaultPointer,
                Resize(DesktopSize {
                    width: 1080,
                    height: 1024,
                }),
            ];
            for op in ops {
                tx.send(op).await.unwrap();
            }
            drop(tx);
            let up = rx.recv().await;
            assert!(matches!(up, Some(DefaultPointer)));
            let up = rx.recv().await;
            assert!(matches!(
                up,
                Some(Resize(DesktopSize {
                    width: 1080,
                    height: 1024
                }))
            ));
            assert!(rx.recv().await.is_none());
        });
    }

    #[test]
    fn test_bitmap() {
        let (tx, rx) = DisplayQueue::channel();
        crate::utils::block_on(async move {
            let ops = vec![
                Bitmap(BitmapUpdate {
                    x: 0,
                    y: 0,
                    stride: NonZero::new(1usize).unwrap(),
                    width: NonZero::new(1024).unwrap(),
                    height: NonZero::new(768).unwrap(),
                    format: PixelFormat::ABgr32,
                    data: Bytes::new(),
                }),
                Bitmap(BitmapUpdate {
                    x: 0,
                    y: 0,
                    stride: NonZero::new(1usize).unwrap(),
                    width: NonZero::new(1024).unwrap(),
                    height: NonZero::new(768).unwrap(),
                    format: PixelFormat::ABgr32,
                    data: Bytes::new(),
                }),
            ];
            for op in ops {
                tx.send(op).await.unwrap();
            }
            drop(tx);
            let up = rx.recv().await;
            assert!(matches!(up, Some(Bitmap(_))));
            assert!(rx.recv().await.is_none());
        });
    }
}
