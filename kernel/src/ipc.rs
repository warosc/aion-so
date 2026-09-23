//! One message, waiting.
//!
//! A process's mailbox holds at most one message, of at most
//! [`CAPACITY`] bytes, in memory of the kernel's
//! (docs/adr/0019-fase3-ipc-v0.md). Everything here is pure: what a
//! mailbox accepts, what it refuses and what comes out of it can be
//! tested without processes, page tables or a boot.
//!
//! The copies in and out happen elsewhere, each one while the process
//! that owns the memory is the one running — which is what makes this a
//! mailbox and not shared memory.

/// The longest message. Small on purpose: a mailbox is filled inside a
/// syscall handler with interrupts off, where nothing asks for memory.
pub const CAPACITY: usize = 64;

/// What a receiver learns about the message it is handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    /// Which slot sent it. A message whose origin is unknown is noise
    /// (ADR 0019, point 8).
    pub from: usize,
    pub len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverError {
    /// There is already a message waiting there.
    Busy,
    /// Longer than a mailbox holds.
    TooLong { capacity: usize },
    /// Nothing at all, which is not a message.
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeError {
    /// Nothing waiting.
    Nothing,
    /// The message does not fit in the buffer offered. It stays where it
    /// is: truncating would lose bytes the sender believes arrived.
    TooLong { len: usize },
}

/// Where a message waits for the process it was sent to.
pub struct Mailbox {
    bytes: [u8; CAPACITY],
    waiting: Option<Delivery>,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Mailbox {
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            waiting: None,
        }
    }

    /// What is waiting, without taking it.
    pub fn waiting(&self) -> Option<Delivery> {
        self.waiting
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_none()
    }

    /// Puts `message` in, if there is room for it and nothing is there.
    pub fn deliver(&mut self, from: usize, message: &[u8]) -> Result<Delivery, DeliverError> {
        if message.is_empty() {
            return Err(DeliverError::Empty);
        }
        if message.len() > CAPACITY {
            return Err(DeliverError::TooLong { capacity: CAPACITY });
        }
        if self.waiting.is_some() {
            return Err(DeliverError::Busy);
        }
        self.bytes[..message.len()].copy_from_slice(message);
        let delivery = Delivery {
            from,
            len: message.len(),
        };
        self.waiting = Some(delivery);
        Ok(delivery)
    }

    /// Takes what is waiting into `into`, and empties the mailbox.
    ///
    /// The mailbox is left as it was on every error, so that a receiver
    /// that came with too small a buffer can come back with a bigger one.
    pub fn take_into(&mut self, into: &mut [u8]) -> Result<Delivery, TakeError> {
        let Some(delivery) = self.waiting else {
            return Err(TakeError::Nothing);
        };
        if into.len() < delivery.len {
            return Err(TakeError::TooLong { len: delivery.len });
        }
        into[..delivery.len].copy_from_slice(&self.bytes[..delivery.len]);
        self.waiting = None;
        Ok(delivery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_comes_out_as_it_went_in() {
        let mut box_of = Mailbox::new();
        assert!(box_of.is_empty());
        assert_eq!(box_of.waiting(), None);

        let sent = b"a message";
        assert_eq!(box_of.deliver(3, sent), Ok(Delivery { from: 3, len: 9 }));
        assert!(!box_of.is_empty());
        assert_eq!(box_of.waiting(), Some(Delivery { from: 3, len: 9 }));

        let mut received = [0u8; CAPACITY];
        assert_eq!(
            box_of.take_into(&mut received),
            Ok(Delivery { from: 3, len: 9 })
        );
        assert_eq!(&received[..9], sent);
        assert!(box_of.is_empty(), "taking it empties the box");
        // And nothing beyond the message was written.
        assert!(received[9..].iter().all(|byte| *byte == 0));
    }

    /// One message at a time: the second sender is told to wait rather
    /// than overwriting what the receiver has not read.
    #[test]
    fn a_full_mailbox_refuses_the_next_message() {
        let mut box_of = Mailbox::new();
        box_of.deliver(0, b"first").unwrap();
        assert_eq!(box_of.deliver(1, b"second"), Err(DeliverError::Busy));

        let mut received = [0u8; CAPACITY];
        let delivery = box_of.take_into(&mut received).unwrap();
        assert_eq!(delivery.from, 0, "the first one is still there");
        assert_eq!(&received[..delivery.len], b"first");
        // With it gone, the next one fits.
        assert!(box_of.deliver(1, b"second").is_ok());
    }

    #[test]
    fn what_a_mailbox_will_not_take() {
        let mut box_of = Mailbox::new();
        assert_eq!(box_of.deliver(0, b""), Err(DeliverError::Empty));
        assert_eq!(
            box_of.deliver(0, &[7; CAPACITY + 1]),
            Err(DeliverError::TooLong { capacity: CAPACITY })
        );
        assert!(box_of.is_empty(), "neither left anything behind");
        // Exactly full is not too long.
        assert!(box_of.deliver(0, &[7; CAPACITY]).is_ok());
    }

    /// A buffer that is too small loses no bytes: the message waits.
    #[test]
    fn a_message_that_does_not_fit_stays_where_it_is() {
        let mut box_of = Mailbox::new();
        box_of.deliver(2, b"twelve bytes").unwrap();

        let mut too_small = [0u8; 4];
        assert_eq!(
            box_of.take_into(&mut too_small),
            Err(TakeError::TooLong { len: 12 })
        );
        assert_eq!(too_small, [0; 4], "and nothing was copied");
        assert!(!box_of.is_empty());

        let mut big_enough = [0u8; 12];
        assert_eq!(
            box_of.take_into(&mut big_enough),
            Ok(Delivery { from: 2, len: 12 })
        );
        assert_eq!(&big_enough, b"twelve bytes");
    }

    #[test]
    fn an_empty_mailbox_hands_out_nothing() {
        let mut box_of = Mailbox::new();
        let mut received = [0u8; CAPACITY];
        assert_eq!(box_of.take_into(&mut received), Err(TakeError::Nothing));
        // Twice, so that asking does not change it.
        assert_eq!(box_of.take_into(&mut received), Err(TakeError::Nothing));
    }
}
