use fehler::{throw, throws};
use rusoto_core::RusotoError;
use rusoto_logs::{
    CloudWatchLogs, CloudWatchLogsClient, InputLogEvent, PutLogEventsError,
    PutLogEventsRequest,
};
use std::time::SystemTime;

/// The maximum number of log events in a batch is 10,000.
pub const MAX_EVENTS_IN_BATCH: usize = 10_000;

/// The maximum batch size is 1,048,576 bytes, and this size is
/// calculated as the sum of all event messages in UTF-8, plus 26
/// bytes for each log event.
pub const MAX_BATCH_SIZE: usize = 1_048_576;

/// The maximum batch size is 1,048,576 bytes, and this size is
/// calculated as the sum of all event messages in UTF-8, plus 26
/// bytes for each log event.
pub const EVENT_OVERHEAD: usize = 26;

/// The time the event occurred, expressed as the number of
/// milliseconds after Jan 1, 1970 00:00:00 UTC.
pub type Timestamp = i64;

/// A batch of log events in a single request cannot span more than 24
/// hours. This constant is in milliseconds.
pub const MAX_DURATION_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// Get the current timestamp. Returns 0 if the time is before the
/// unix epoch.
pub fn get_current_timestamp() -> Timestamp {
    if let Ok(duration) =
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
    {
        duration.as_millis() as Timestamp
    } else {
        0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("event exceeds the max batch size")]
    EventTooLarge(usize),
    #[error("failed to upload log batch: {0}")]
    PutLogsError(#[from] RusotoError<PutLogEventsError>),
}

pub type Batch = PutLogEventsRequest;

#[derive(Default)]
pub struct QueuedBatches {
    /// Queued batches that haven't been sent yet.
    ///
    /// Box the batch so that adding and removing elements from the
    /// vector is cheap.
    #[allow(clippy::vec_box)]
    batches: Vec<Box<Batch>>,

    /// Total size of the batch at the end of the batches vector.
    current_batch_size: usize,
}

impl QueuedBatches {
    /// Add a new event.
    ///
    /// There are a couple AWS limits not enforced yet:
    ///
    /// - None of the log events in the batch can be more than 2 hours
    ///   in the future
    ///
    /// - None of the log events in the batch can be older than 14 days
    ///   or older than the retention period of the log group
    #[throws]
    pub fn add_event(&mut self, event: InputLogEvent) {
        let event_size = event.message.as_bytes().len() + EVENT_OVERHEAD;
        if event_size > MAX_BATCH_SIZE {
            // The event is bigger than the batch size
            throw!(Error::EventTooLarge(event_size));
        }

        if self.is_new_batch_needed(&event, event_size) {
            self.batches.push(Box::new(Batch::default()));
            self.current_batch_size = 0;
        }

        // Ok to unwrap here, the code above ensures there is at least
        // one available batch
        let batch = self.batches.last_mut().unwrap();
        batch.log_events.push(event);
        self.current_batch_size += event_size;
    }

    fn is_new_batch_needed(
        &self,
        event: &InputLogEvent,
        event_size: usize,
    ) -> bool {
        let batch = if let Some(batch) = self.batches.last() {
            batch
        } else {
            // Ensure there's at least one batch
            return true;
        };

        if batch.log_events.len() >= MAX_EVENTS_IN_BATCH {
            // Maximum number of events exceeded
            return true;
        }

        if self.current_batch_size + event_size > MAX_BATCH_SIZE {
            // Maximum payload size exceeded
            return true;
        }

        if let Some(last_event) = batch.log_events.last() {
            if last_event.timestamp > event.timestamp {
                // Timestamp is not in-order
                return true;
            }
        }

        if let Some(first_event) = batch.log_events.first() {
            // Time between the new event and the first event in the batch
            let duration_millis = event.timestamp - first_event.timestamp;
            if duration_millis > MAX_DURATION_MILLIS {
                // Batch cannot span more than 24 hours
                return true;
            }
        }

        false
    }
}

pub struct BatchUploader {
    queued_batches: QueuedBatches,

    client: CloudWatchLogsClient,
    next_sequence_token: Option<String>,
}

impl BatchUploader {
    /// Add a new event.
    ///
    /// There are a couple AWS limits not enforced yet:
    ///
    /// - None of the log events in the batch can be more than 2 hours
    ///   in the future
    ///
    /// - None of the log events in the batch can be older than 14 days
    ///   or older than the retention period of the log group
    #[throws]
    pub fn add_event(&mut self, event: InputLogEvent) {
        self.queued_batches.add_event(event)?;
    }

    #[throws]
    pub fn upload_batch(&mut self) {
        let mut batch = if let Some(batch) = self.queued_batches.batches.pop() {
            batch
        } else {
            return;
        };

        batch.sequence_token = self.next_sequence_token.clone();
        // TODO: add log/stream names

        match self.client.put_log_events(*batch).sync() {
            Ok(resp) => {
                self.next_sequence_token = resp.next_sequence_token;

                // TODO: handle rejected events
            }
            Err(err) => {
                // TODO: if the batch upload failed, consider putting
                // the batch back. Unfortunately this requires cloning
                // the batch.

                // TODO: I assume that if this happens we need to refresh the
                // sequence token

                throw!(err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_too_large() {
        let mut qb = QueuedBatches::default();

        // Create a message that is at the limit
        let max_message_size = MAX_BATCH_SIZE - EVENT_OVERHEAD;
        let mut message = String::with_capacity(max_message_size + 1);
        for _ in 0..message.capacity() - 1 {
            message.push('x');
        }

        // Verify the message is added successfully
        qb.add_event(InputLogEvent {
            message: message.clone(),
            timestamp: 0,
        })
        .unwrap();

        // Make the message too big
        message.push('x');
        assert!(matches!(
            qb.add_event(InputLogEvent {
                message,
                timestamp: 0,
            }),
            Err(Error::EventTooLarge(size)) if size == MAX_BATCH_SIZE + 1
        ));
    }

    #[test]
    fn test_max_events_in_batch() {
        let mut qb = QueuedBatches::default();
        for _ in 0..MAX_EVENTS_IN_BATCH {
            qb.add_event(InputLogEvent {
                ..Default::default()
            })
            .unwrap();
            // All of these events should fit in one batch
            assert_eq!(qb.batches.len(), 1);
        }

        // Verify that adding one more event creates a new batch
        qb.add_event(InputLogEvent {
            ..Default::default()
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 2);
    }

    #[test]
    fn test_max_batch_size() {
        let mut qb = QueuedBatches::default();

        // Create a message slightly under the limit
        let message_size = MAX_BATCH_SIZE - EVENT_OVERHEAD * 2;
        let mut message = String::with_capacity(message_size);
        for _ in 0..message.capacity() {
            message.push('x');
        }

        // Verify the message is added successfully
        qb.add_event(InputLogEvent {
            message: message.clone(),
            timestamp: 0,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 1);
        assert_eq!(qb.current_batch_size, message_size + EVENT_OVERHEAD);

        // Verify that adding one more message within the batch is OK
        qb.add_event(InputLogEvent {
            message: "".to_string(),
            timestamp: 0,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 1);
        assert_eq!(qb.current_batch_size, message_size + EVENT_OVERHEAD * 2);

        // Verify that adding anything else goes into a new batch
        qb.add_event(InputLogEvent {
            message: "".to_string(),
            timestamp: 0,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 2);
        assert_eq!(qb.current_batch_size, EVENT_OVERHEAD);
    }

    #[test]
    fn test_timestamp_order() {
        let mut qb = QueuedBatches::default();

        // Add an event at time 1
        qb.add_event(InputLogEvent {
            message: "".to_string(),
            timestamp: 1,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 1);

        // Add an event at time 0, verify it goes into a new batch
        qb.add_event(InputLogEvent {
            message: "".to_string(),
            timestamp: 0,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 2);
    }

    #[test]
    fn test_batch_max_duration() {
        let mut qb = QueuedBatches::default();

        // Add an event at time 0
        qb.add_event(InputLogEvent {
            message: "".to_string(),
            timestamp: 0,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 1);

        // Add an event over 24 hours later, verify it goes into a new batch
        qb.add_event(InputLogEvent {
            message: "".to_string(),
            timestamp: MAX_DURATION_MILLIS + 1,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 2);
    }
}
