//! Drop-oldest queue for decoded video frames.

use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use tokio::sync::Notify;

/// Number of decoded frames buffered before the oldest is dropped.
pub const VIDEO_QUEUE_CAPACITY: usize = 2;

/// A decoded RGBA8 frame.
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Tightly packed RGBA8 pixels, `width * height * 4` bytes.
    pub pixels: Arc<[u8]>,
}

/// Bounded queue of decoded frames (capacity 2). Pushing into a full queue drops the
/// oldest frame, so a slow UI always shows the most recent picture.
#[derive(Debug, Clone)]
pub struct VideoFrameQueue {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    frames: ArrayQueue<RgbaFrame>,
    notify: Notify,
}

impl Default for VideoFrameQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoFrameQueue {
    /// Creates an empty queue.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                frames: ArrayQueue::new(VIDEO_QUEUE_CAPACITY),
                notify: Notify::new(),
            }),
        }
    }

    /// Pushes a frame, dropping the oldest one if the queue is full.
    pub fn push(&self, frame: RgbaFrame) {
        let _dropped = self.inner.frames.force_push(frame);
        self.inner.notify.notify_one();
    }

    /// Takes the oldest frame without waiting.
    pub fn try_pop(&self) -> Option<RgbaFrame> {
        self.inner.frames.pop()
    }

    /// Waits for the next frame.
    pub async fn next(&self) -> RgbaFrame {
        loop {
            if let Some(frame) = self.inner.frames.pop() {
                return frame;
            }
            self.inner.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32) -> RgbaFrame {
        RgbaFrame {
            width,
            height: 1,
            pixels: Arc::from(vec![0u8; width as usize * 4]),
        }
    }

    #[test]
    fn drops_oldest_when_full() {
        let q = VideoFrameQueue::new();
        q.push(frame(1));
        q.push(frame(2));
        q.push(frame(3));
        assert_eq!(q.try_pop().map(|f| f.width), Some(2));
        assert_eq!(q.try_pop().map(|f| f.width), Some(3));
        assert!(q.try_pop().is_none());
    }

    #[tokio::test]
    async fn next_wakes_on_push() {
        let q = VideoFrameQueue::new();
        let q2 = q.clone();
        let waiter = tokio::spawn(async move { q2.next().await.width });
        tokio::task::yield_now().await;
        q.push(frame(7));
        assert_eq!(waiter.await.unwrap(), 7);
    }
}
