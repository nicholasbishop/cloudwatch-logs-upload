use cloudwatch_logs_upload::{
    get_current_timestamp, BatchUploader, UploadTarget,
};
use rand::Rng;
use rusoto_core::{Region, RusotoError};
use rusoto_logs::{
    CloudWatchLogs, CloudWatchLogsClient, CreateLogStreamError,
    CreateLogStreamRequest, InputLogEvent,
};
use std::{env, thread, time};

/// Create the log stream (the log group is assumed to already exist).
fn create_log_stream(client: &CloudWatchLogsClient, group: &str, stream: &str) {
    let resp = client
        .create_log_stream(CreateLogStreamRequest {
            log_group_name: group.into(),
            log_stream_name: stream.into(),
        })
        .sync();
    if let Err(err) = resp {
        if !matches!(
            err,
            RusotoError::Service(CreateLogStreamError::ResourceAlreadyExists(
                _
            ),)
        ) {
            panic!("failed to create log stream: {}", err);
        }
    }
}

fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let args = env::args().collect::<Vec<_>>();
    if args.len() <= 2 {
        eprintln!("required arguments: log-group-name log-stream-name");
        return;
    }
    let target = UploadTarget {
        group: args[1].clone(),
        stream: args[2].clone(),
    };
    let client = CloudWatchLogsClient::new(Region::default());
    create_log_stream(&client, &target.group, &target.stream);
    let uploader = BatchUploader::new(client, target);
    let handle = uploader
        .start_background_thread()
        .expect("failed to start background thread");

    let mut thread_handles = Vec::new();
    thread_handles.push(handle);

    // Start several threads that upload events
    for i in 0..4 {
        // The uploader has an internal Arc+Mutex, so it's safe to
        // clone the uploader and move the clone into a new thread.
        let uploader = uploader.clone();
        let handle = thread::spawn(move || {
            let mut rng = rand::thread_rng();
            loop {
                thread::sleep(time::Duration::from_millis(
                    rng.gen_range(0, 1000),
                ));

                uploader
                    .add_event(InputLogEvent {
                        message: format!("hello from thread {}", i),
                        timestamp: get_current_timestamp(),
                    })
                    .unwrap();
            }
        });
        thread_handles.push(handle);
    }

    for handle in thread_handles {
        handle.join().expect("failed to join thread handle");
    }
}
