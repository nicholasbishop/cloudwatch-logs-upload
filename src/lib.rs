//! Rust library for uploading events to AWS CloudWatch Logs.

#![warn(missing_docs)]

use fehler::{throw, throws};
use log::error;
use rusoto_core::RusotoError;
use rusoto_logs::{
    CloudWatchLogs, CloudWatchLogsClient, DescribeLogStreamsError,
    DescribeLogStreamsRequest, InputLogEvent, PutLogEventsError,
    PutLogEventsRequest,
};
use std::{
    io,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};

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

/// Unified event type for upload errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Event message string is larger than 1,048,550 bytes.
    #[error("event exceeds the max batch size")]
    EventTooLarge(usize),

    /// An API error was returned when trying to upload a batch.
    #[error("failed to upload log batch: {0}")]
    PutLogsError(#[from] RusotoError<PutLogEventsError>),

    /// An API error was returned when trying to get the log stream
    /// upload sequence token.
    #[error("failed to get sequence token: {0}")]
    SequenceTokenError(#[from] RusotoError<DescribeLogStreamsError>),

    /// The log stream does not exist.
    #[error("invalid log stream")]
    InvalidLogStream,

    /// An internal mutex error occurred.
    #[error("failed to lock the mutex")]
    PoisonedLock,

    /// The background upload thread can only be started once.
    #[error("upload thread already started")]
    ThreadAlreadyStarted,

    /// Failed to start the background upload thread.
    #[error("failed to spawn thread: {0}")]
    SpawnError(io::Error),
}

/// An inclusive range of timestamps.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TimestampRange {
    /// Start time (inclusive).
    pub start: Timestamp,
    /// End time (inclusive).
    pub end: Timestamp,
}

impl TimestampRange {
    /// Create a range for a single instant in time.
    pub fn new(t: Timestamp) -> TimestampRange {
        TimestampRange { start: t, end: t }
    }

    /// Time in milliseconds between start and end.
    pub fn duration_in_millis(&self) -> i64 {
        self.end - self.start
    }

    /// Adjust start and end as needed to include `t`.
    pub fn expand_to_include(&mut self, t: Timestamp) {
        if t < self.start {
            self.start = t;
        }
        if t > self.end {
            self.end = t;
        }
    }

    /// Create a new TimestampRange with start and end adjusted as
    /// needed to include `t`.
    pub fn expand_to_include_copy(&self, t: Timestamp) -> TimestampRange {
        let mut copy = *self;
        copy.expand_to_include(t);
        copy
    }
}

/// Internal interface for creating batches of events.
///
/// This shouldn't normally need to be used directly, but is exposed
/// in case the BatchUploader interface doesn't behave quite the way
/// you want, you might still find value in using QueuedBatches to
/// handle the various batch upload limits.
#[derive(Default)]
pub struct QueuedBatches {
    /// Queued batches that haven't been sent yet.
    ///
    /// Box the batch so that adding and removing elements from the
    /// batches vector is cheap.
    ///
    /// Events are queued up in a vector that is not necessarily in
    /// order. CloudWatch Logs requires events to be sorted by
    /// timestamp, but in practice if you have a lot of events coming
    /// in from different threads they will end up being out of order
    /// due to waiting on a shared lock. So if you try to keep the
    /// events in each batch in order, and start a new batch every
    /// time an out-of-order event is received, the batches end up
    /// being very small.
    #[allow(clippy::vec_box)]
    batches: Vec<Box<Vec<InputLogEvent>>>,

    /// Total size of the batch at the end of the batches vector.
    current_batch_size: usize,

    /// Oldest and newest timestamps of events in the batch at the end
    /// of the batches vector. This is used to ensure that a batch
    /// does not exceed the 24-hour limit.
    current_batch_time_range: TimestampRange,
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
            self.batches.push(Box::new(Vec::new()));
            self.current_batch_size = 0;
            self.current_batch_time_range =
                TimestampRange::new(event.timestamp);
        }

        self.current_batch_size += event_size;
        self.current_batch_time_range
            .expand_to_include(event.timestamp);
        // Ok to unwrap here, the code above ensures there is at least
        // one available batch
        let batch = self.batches.last_mut().unwrap();
        batch.push(event);
    }

    fn is_new_batch_needed(
        &self,
        event: &InputLogEvent,
        event_size: usize,
    ) -> bool {
        // Ensure there's at least one batch
        let batch = if let Some(batch) = self.batches.last() {
            batch
        } else {
            return true;
        };

        // Check if maximum number of events exceeded
        if batch.len() >= MAX_EVENTS_IN_BATCH {
            return true;
        }

        // Check if maximum payload size exceeded
        if self.current_batch_size + event_size > MAX_BATCH_SIZE {
            return true;
        }

        // Check if 24-hour limit is exceeded
        if !batch.is_empty() {
            let new_range = self
                .current_batch_time_range
                .expand_to_include_copy(event.timestamp);
            if new_range.duration_in_millis() > MAX_DURATION_MILLIS {
                return true;
            }
        }

        false
    }
}

/// Log group and stream names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadTarget {
    /// Log group name.
    pub group: String,
    /// Log stream name.
    pub stream: String,
}

struct BatchUploaderInternal {
    target: UploadTarget,

    queued_batches: QueuedBatches,

    client: CloudWatchLogsClient,
    next_sequence_token: Option<String>,

    thread_started: bool,
}

impl BatchUploaderInternal {
    #[throws]
    fn refresh_sequence_token(&mut self) {
        let resp = self
            .client
            .describe_log_streams(DescribeLogStreamsRequest {
                limit: Some(1),
                order_by: Some("LogStreamName".into()),
                log_group_name: self.target.group.clone(),
                log_stream_name_prefix: Some(self.target.stream.clone()),
                ..Default::default()
            })
            .sync()?;
        let log_streams = resp.log_streams.ok_or(Error::InvalidLogStream)?;
        // TODO: need to verify that this is correct if you have
        // multiple log streams with the same prefix. I *think* it's
        // good because we are sorting by stream name and limiting to
        // 1, so as long as "myPrefix" gets sorted before
        // "myPrefixAndOtherStuff" this should be correct. Needs
        // testing though.
        let log_stream = log_streams.first().ok_or(Error::InvalidLogStream)?;
        if Some(self.target.stream.clone()) != log_stream.log_stream_name {
            // This should never happen
            error!(
                "log stream name {} != {:?}",
                self.target.stream, log_stream.log_stream_name
            );
            throw!(Error::InvalidLogStream);
        }
        self.next_sequence_token = log_stream.upload_sequence_token.clone();
    }

    #[throws]
    fn upload_batch(&mut self) {
        let mut batch = if let Some(batch) = self.queued_batches.batches.pop() {
            *batch
        } else {
            return;
        };

        // Refresh the sequence token if necessary. This actually
        // isn't needed right after creating the log stream, so
        // there's an unnecessary fetch here the first time, but
        // that's probably fine.
        if self.next_sequence_token.is_none() {
            self.refresh_sequence_token()?;
        }

        // The events must be sorted by timestamp
        batch.sort_unstable_by_key(|event| event.timestamp);

        let req = PutLogEventsRequest {
            log_events: batch,
            sequence_token: self.next_sequence_token.clone(),
            log_group_name: self.target.group.clone(),
            log_stream_name: self.target.stream.clone(),
        };

        match self.client.put_log_events(req).sync() {
            Ok(resp) => {
                self.next_sequence_token = resp.next_sequence_token;

                // TODO: handle rejected events
            }
            Err(err) => {
                // TODO: if the batch upload failed, consider putting
                // the batch back.

                // Clear the sequence token so that it gets refreshed
                // next time
                self.next_sequence_token = None;

                throw!(err);
            }
        }
    }
}

/// Main interface for uploading logs in batches to AWS CloudWatch Logs.
///
/// This can be safely used from multiple threads by cloning it.
#[derive(Clone)]
pub struct BatchUploader {
    internal: Arc<Mutex<BatchUploaderInternal>>,
}

impl BatchUploader {
    /// Create a new `BatchUploader`.
    pub fn new(
        client: CloudWatchLogsClient,
        target: UploadTarget,
    ) -> BatchUploader {
        BatchUploader {
            internal: Arc::new(Mutex::new(BatchUploaderInternal {
                target,
                client,
                queued_batches: QueuedBatches::default(),
                next_sequence_token: None,
                thread_started: false,
            })),
        }
    }

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
    pub fn add_event(&self, event: InputLogEvent) {
        let mut guard =
            self.internal.lock().map_err(|_| Error::PoisonedLock)?;
        guard.queued_batches.add_event(event)?;
    }

    /// Start a background thread for uploading batches of events.
    #[throws]
    pub fn start_background_thread(&self) -> thread::JoinHandle<()> {
        let mut guard =
            self.internal.lock().map_err(|_| Error::PoisonedLock)?;
        // Prevent multiple upload threads from being started
        if guard.thread_started {
            throw!(Error::ThreadAlreadyStarted);
        }
        guard.thread_started = true;

        let builder =
            thread::Builder::new().name("cloudwatch-logs-upload".into());
        let internal = self.internal.clone();
        let handle = builder
            .spawn(move || loop {
                if let Ok(mut guard) = internal.lock() {
                    // There is a quota of 5 requests per second per log
                    // stream, so upload up to five batches, then sleep
                    // for at least one second.
                    for _ in 0..5 {
                        if let Err(err) = guard.upload_batch() {
                            error!(
                                "CloudWatch Logs batch upload failed: {}",
                                err
                            );
                        }
                    }
                } else {
                    error!("CloudWatch Logs bad lock");
                }
                thread::sleep(Duration::from_secs(1));
            })
            .map_err(Error::SpawnError)?;
        handle
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

        // Add an event at time 0, verify it goes into the same batch
        qb.add_event(InputLogEvent {
            message: "".to_string(),
            timestamp: 0,
        })
        .unwrap();
        assert_eq!(qb.batches.len(), 1);
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
