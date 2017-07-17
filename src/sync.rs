use std::collections::VecDeque;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::{Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::atomic::Ordering::{Acquire, Release, Relaxed, SeqCst};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use super::SendError;
use super::TrySendError;
use super::SendTimeoutError;
use super::RecvError;
use super::TryRecvError;
use super::RecvTimeoutError;

// TODO: optimize if there's a single Sender or a single Receiver

struct Node<T> {
    lap: AtomicUsize,
    value: T,
}

struct Inner {
    senders: VecDeque<Thread>,
    receivers: VecDeque<Thread>,
}

#[repr(C)]
pub struct Queue<T> {
    buffer: *mut UnsafeCell<Node<T>>,
    cap: usize,
    power: usize,
    _pad0: [u8; 64],
    head: AtomicUsize,
    _pad1: [u8; 64],
    tail: AtomicUsize,
    _pad2: [u8; 64],
    closed: AtomicBool,

    lock: Mutex<Inner>,
    senders_len: AtomicUsize,
    receivers_len: AtomicUsize,

    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub fn with_capacity(cap: usize) -> Self {
        assert!(cap > 0);
        assert!(cap <= ::std::isize::MAX as usize);

        let power = cap.next_power_of_two();

        let mut v = Vec::with_capacity(cap);
        let buffer = v.as_mut_ptr();
        mem::forget(v);
        unsafe { ptr::write_bytes(buffer, 0, cap) }

        Queue {
            buffer: buffer,
            cap: cap,
            power: power,
            _pad0: unsafe { mem::uninitialized() },
            head: AtomicUsize::new(power),
            _pad1: unsafe { mem::uninitialized() },
            tail: AtomicUsize::new(0),
            _pad2: unsafe { mem::uninitialized() },
            closed: AtomicBool::new(false),

            lock: Mutex::new(Inner {
                senders: VecDeque::new(),
                receivers: VecDeque::new(),
            }),
            senders_len: AtomicUsize::new(0),
            receivers_len: AtomicUsize::new(0),

            _marker: PhantomData,
        }
    }

    fn try_send_inner<'a>(&'a self, value: T, lock: Option<&mut MutexGuard<'a, Inner>>) -> Result<(), TrySendError<T>> {
        if self.closed.load(SeqCst) {
            return Err(TrySendError::Disconnected(value));
        }

        let cap = self.cap;
        let power = self.power;
        let buffer = self.buffer;

        loop {
            let tail = self.tail.load(SeqCst);
            let pos = tail & (power - 1);
            let lap = tail & !(power - 1);

            let cell = unsafe { (*buffer.offset(pos as isize)).get() };
            let clap = unsafe { (*cell).lap.load(SeqCst) };

            if lap == clap {
                let new = if pos + 1 < cap {
                    tail + 1
                } else {
                    lap.wrapping_add(power).wrapping_add(power)
                };

                if self.tail.compare_exchange_weak(tail, new, SeqCst, Relaxed).is_ok() {
                    unsafe {
                        (*cell).value = value;
                        (*cell).lap.store(clap.wrapping_add(power), Release);

                        if self.receivers_len.load(SeqCst) > 0 {
                            let mut l;
                            let lock = match lock {
                                None => {
                                    l = self.lock.lock();
                                    l.as_mut().unwrap()
                                }
                                Some(x) => x,
                            };

                            if let Some(t) = lock.receivers.pop_front() {
                                self.receivers_len.store(lock.receivers.len(), SeqCst);
                                t.unpark();
                            }
                        }
                        return Ok(());
                    }
                }
            } else if clap.wrapping_add(power) == lap {
                return Err(TrySendError::Full(value));
            }
        }
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.try_send_inner(value, None)
    }

    fn send_until(
        &self,
        mut value: T,
        deadline: Option<Instant>,
    ) -> Result<(), SendTimeoutError<T>> {
        loop {
            match self.try_send(value) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(v)) => return Err(SendTimeoutError::Disconnected(v)),
                Err(TrySendError::Full(v)) => value = v,
            }

            let now = Instant::now();
            if let Some(end) = deadline {
                if now >= end {
                    return Err(SendTimeoutError::Timeout(value));
                }
            }

            // Register a sender.
            {
                let mut lock = self.lock.lock().unwrap();
                lock.senders.push_back(thread::current());
                self.senders_len.store(lock.senders.len(), SeqCst);

                match self.try_send_inner(value, Some(&mut lock)) {
                    Ok(v) => {
                        lock.senders.pop_back();
                        self.senders_len.store(lock.senders.len(), SeqCst);
                        return Ok(());
                    }
                    Err(TrySendError::Disconnected(v)) => {
                        lock.senders.pop_back();
                        self.senders_len.store(lock.senders.len(), SeqCst);
                        return Err(SendTimeoutError::Disconnected(v));
                    }
                    Err(TrySendError::Full(v)) => value = v,
                }
            }

            if let Some(end) = deadline {
                thread::park_timeout(end - now);
            } else {
                thread::park();
            }

            let mut lock = self.lock.lock().unwrap();
            let id = thread::current().id();

            if let Some((i, _)) = lock.senders.iter().enumerate().find(|&(_, t)| t.id() == id) {
                lock.senders.remove(i);
                self.senders_len.store(lock.senders.len(), SeqCst);
            } else if let Some(t) = lock.senders.pop_front() {
                self.senders_len.store(lock.senders.len(), SeqCst);
                t.unpark();
            }
        }
    }

    pub fn send_timeout(&self, value: T, dur: Duration) -> Result<(), SendTimeoutError<T>> {
        self.send_until(value, Some(Instant::now() + dur))
    }

    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        match self.send_until(value, None) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Disconnected(v)) => Err(SendError(v)),
            Err(SendTimeoutError::Timeout(v)) => Err(SendError(v)),
        }
    }

    fn try_recv_inner<'a>(&'a self, lock: Option<&mut MutexGuard<'a, Inner>>) -> Result<T, TryRecvError> {
        let cap = self.cap;
        let power = self.power;
        let buffer = self.buffer;

        loop {
            let head = self.head.load(SeqCst);
            let pos = head & (power - 1);
            let lap = head & !(power - 1);

            let cell = unsafe { (*buffer.offset(pos as isize)).get() };
            let clap = unsafe { (*cell).lap.load(SeqCst) };

            if lap == clap {
                let new = if pos + 1 < cap {
                    head + 1
                } else {
                    lap.wrapping_add(power).wrapping_add(power)
                };

                if self.head.compare_exchange_weak(head, new, SeqCst, Relaxed).is_ok() {
                    unsafe {
                        let value = ptr::read(&(*cell).value);
                        (*cell).lap.store(clap.wrapping_add(power), Release);

                        if self.senders_len.load(SeqCst) > 0 {
                            let mut l;
                            let lock = match lock {
                                None => {
                                    l = self.lock.lock();
                                    l.as_mut().unwrap()
                                }
                                Some(x) => x,
                            };

                            if let Some(t) = lock.senders.pop_front() {
                                self.senders_len.store(lock.senders.len(), SeqCst);
                                t.unpark();
                            }
                        }
                        return Ok(value);
                    }
                }
            } else if clap.wrapping_add(power) == lap {
                if self.closed.load(SeqCst) {
                    return Err(TryRecvError::Disconnected);
                } else {
                    return Err(TryRecvError::Empty);
                }
            }
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.try_recv_inner(None)
    }

    fn recv_until(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        loop {
            match self.try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => {},
            }

            let now = Instant::now();
            if let Some(end) = deadline {
                if now >= end {
                    return Err(RecvTimeoutError::Timeout);
                }
            }

            // Register a receiver.
            {
                let mut lock = self.lock.lock().unwrap();
                lock.receivers.push_back(thread::current());
                self.receivers_len.store(lock.receivers.len(), SeqCst);

                match self.try_recv_inner(Some(&mut lock)) {
                    Ok(v) => {
                        lock.receivers.pop_back();
                        self.receivers_len.store(lock.receivers.len(), SeqCst);
                        return Ok(v);
                    }
                    Err(TryRecvError::Disconnected) => {
                        lock.receivers.pop_back();
                        self.receivers_len.store(lock.receivers.len(), SeqCst);
                        return Err(RecvTimeoutError::Disconnected);
                    }
                    Err(TryRecvError::Empty) => {}
                }
            }

            if let Some(end) = deadline {
                thread::park_timeout(end - now);
            } else {
                thread::park();
            }

            let mut lock = self.lock.lock().unwrap();
            let id = thread::current().id();

            if let Some((i, _)) = lock.receivers.iter().enumerate().find(|&(_, t)| t.id() == id) {
                lock.receivers.remove(i);
                self.receivers_len.store(lock.receivers.len(), SeqCst);
            } else if let Some(t) = lock.receivers.pop_front() {
                self.receivers_len.store(lock.receivers.len(), SeqCst);
                t.unpark();
            }
        }
    }

    pub fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_until(Some(Instant::now() + dur))
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        if let Ok(v) = self.recv_until(None) {
            Ok(v)
        } else {
            Err(RecvError)
        }
    }

    pub fn close(&self) -> bool {
        if self.closed.load(SeqCst) {
            return false;
        }

        let mut lock = self.lock.lock().unwrap();

        if self.closed.swap(true, SeqCst) {
            return false;
        }

        self.senders_len.store(0, SeqCst);
        self.receivers_len.store(0, SeqCst);

        for t in lock.senders.drain(..) {
            t.unpark();
        }
        for t in lock.receivers.drain(..) {
            t.unpark();
        }

        true
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(SeqCst)
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        if cfg!(debug_assertions) {
            let mut lock = self.lock.get_mut().unwrap();
            debug_assert_eq!(lock.senders.len(), 0);
            debug_assert_eq!(lock.receivers.len(), 0);
            debug_assert_eq!(self.senders_len.load(SeqCst), 0);
            debug_assert_eq!(self.receivers_len.load(SeqCst), 0);
        }

        // TODO: impl Drop
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering::SeqCst;
    use std::thread;

    use crossbeam;

    use super::*;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    // TODO: drop test

    #[test]
    fn smoke() {
        let q = Queue::with_capacity(1);
        q.try_send(7).unwrap();
        assert_eq!(q.try_recv().unwrap(), 7);

        q.send(8);
        assert_eq!(q.recv().unwrap(), 8);

        assert_eq!(q.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(q.recv_timeout(ms(100)), Err(RecvTimeoutError::Timeout));
    }

    #[test]
    fn recv() {
        let q = Queue::with_capacity(100);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.recv(), Ok(7));
                thread::sleep(ms(100));
                assert_eq!(q.recv(), Ok(8));
                thread::sleep(ms(100));
                assert_eq!(q.recv(), Ok(9));
                assert_eq!(q.recv(), Err(RecvError));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                assert_eq!(q.send(7), Ok(()));
                assert_eq!(q.send(8), Ok(()));
                assert_eq!(q.send(9), Ok(()));
                q.close();
            });
        });
    }

    #[test]
    fn recv_timeout() {
        let q = Queue::with_capacity(100);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.recv_timeout(ms(100)), Err(RecvTimeoutError::Timeout));
                assert_eq!(q.recv_timeout(ms(100)), Ok(7));
                assert_eq!(q.recv_timeout(ms(100)), Err(RecvTimeoutError::Disconnected));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                assert_eq!(q.send(7), Ok(()));
                q.close();
            });
        });
    }

    #[test]
    fn try_recv() {
        let q = Queue::with_capacity(100);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.try_recv(), Err(TryRecvError::Empty));
                thread::sleep(ms(150));
                assert_eq!(q.try_recv(), Ok(7));
                thread::sleep(ms(50));
                assert_eq!(q.try_recv(), Err(TryRecvError::Disconnected));
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                assert_eq!(q.send(7), Ok(()));
                q.close();
            });
        });
    }

    #[test]
    fn send() {
        let q = Queue::with_capacity(1);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.send(7), Ok(()));
                thread::sleep(ms(100));
                assert_eq!(q.send(8), Ok(()));
                thread::sleep(ms(100));
                assert_eq!(q.send(9), Ok(()));
                thread::sleep(ms(100));
                assert_eq!(q.send(10), Err(SendError(10)));
            });
            s.spawn(|| {
                thread::sleep(ms(150));
                assert_eq!(q.recv(), Ok(7));
                assert_eq!(q.recv(), Ok(8));
                assert_eq!(q.recv(), Ok(9));
                q.close();
            });
        });
    }

    #[test]
    fn send_timeout() {
        let q = Queue::with_capacity(2);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.send_timeout(1, ms(100)), Ok(()));
                assert_eq!(q.send_timeout(2, ms(100)), Ok(()));
                assert_eq!(q.send_timeout(3, ms(50)), Err(SendTimeoutError::Timeout(3)));
                thread::sleep(ms(100));
                assert_eq!(q.send_timeout(4, ms(100)), Ok(()));
                thread::sleep(ms(100));
                assert_eq!(q.send(5), Err(SendError(5)));
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                assert_eq!(q.recv(), Ok(1));
                thread::sleep(ms(100));
                assert_eq!(q.recv(), Ok(2));
                assert_eq!(q.recv(), Ok(4));
                q.close();
            });
        });
    }

    #[test]
    fn try_send() {
        let q = Queue::with_capacity(1);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.try_send(1), Ok(()));
                assert_eq!(q.try_send(2), Err(TrySendError::Full(2)));
                thread::sleep(ms(150));
                assert_eq!(q.try_send(3), Ok(()));
                thread::sleep(ms(50));
                assert_eq!(q.try_send(4), Err(TrySendError::Disconnected(4)));
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                assert_eq!(q.try_recv(), Ok(1));
                assert_eq!(q.try_recv(), Err(TryRecvError::Empty));
                assert_eq!(q.recv(), Ok(3));
                q.close();
            });
        });
    }

    #[test]
    fn is_closed() {
        let q = Queue::<()>::with_capacity(100);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert!(!q.is_closed());
                thread::sleep(ms(150));
                assert!(q.is_closed());
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                assert!(!q.is_closed());
                q.close();
                assert!(q.is_closed());
            });
        });
    }

    #[test]
    fn recv_after_close() {
        let q = Queue::with_capacity(100);

        q.send(1).unwrap();
        q.send(2).unwrap();
        q.send(3).unwrap();

        q.close();

        assert_eq!(q.recv(), Ok(1));
        assert_eq!(q.recv(), Ok(2));
        assert_eq!(q.recv(), Ok(3));
        assert_eq!(q.recv(), Err(RecvError));
    }

    #[test]
    fn close_signals_sender() {
        let q = Queue::with_capacity(1);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.send(()), Ok(()));
                assert_eq!(q.send(()), Err(SendError(())));
                assert!(q.is_closed());
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                q.close();
            });
        });
    }

    #[test]
    fn close_signals_receiver() {
        let q = Queue::<()>::with_capacity(1);

        crossbeam::scope(|s| {
            s.spawn(|| {
                assert_eq!(q.recv(), Err(RecvError));
                assert!(q.is_closed());
            });
            s.spawn(|| {
                thread::sleep(ms(100));
                q.close();
            });
        });
    }

    #[test]
    fn spsc() {
        const COUNT: usize = 100_000;

        let q = Queue::with_capacity(3);

        crossbeam::scope(|s| {
            s.spawn(|| {
                for i in 0..COUNT {
                    assert_eq!(q.recv(), Ok(i));
                }
                assert_eq!(q.recv(), Err(RecvError));
            });
            s.spawn(|| {
                for i in 0..COUNT {
                    q.send(i).unwrap();
                }
                q.close();
            });
        });
    }

    #[test]
    fn mpmc() {
        const COUNT: usize = 25_000;
        const THREADS: usize = 4;

        let q = Queue::<usize>::with_capacity(3);
        let v = (0..COUNT).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();

        crossbeam::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(|| {
                    for i in 0..COUNT {
                        let n = q.recv().unwrap();
                        v[n].fetch_add(1, SeqCst);
                    }
                });
            }
            for _ in 0..THREADS {
                s.spawn(|| {
                    for i in 0..COUNT {
                        q.send(i).unwrap();
                    }
                });
            }
        });

        for c in v {
            assert_eq!(c.load(SeqCst), THREADS);
        }
    }
}
