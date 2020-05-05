use cloudwatch_logs_upload::{
    get_current_timestamp, BatchUploader, UploadTarget,
};
use rusoto_core::Region;
use rusoto_logs::{CloudWatchLogsClient, InputLogEvent};
use std::{env, thread, time};

fn main() {
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
    let mut uploader = BatchUploader::new(client, target);
    uploader.start_background_thread();
    loop {
        uploader
            .add_event(InputLogEvent {
                message: "hello there".into(),
                timestamp: get_current_timestamp(),
            })
            .unwrap();
        thread::sleep(time::Duration::from_secs(2));
    }
}
