**This tool is no longer under active development. If you are interested in taking over or repurposing the name on crates.io, feel free to contact me: nbishop@nbishop.net**

# cloudwatch-logs-upload

[![crates.io](https://img.shields.io/crates/v/cloudwatch-logs-upload.svg)](https://crates.io/crates/cloudwatch-logs-upload)
[![Documentation](https://docs.rs/cloudwatch-logs-upload/badge.svg)](https://docs.rs/cloudwatch-logs-upload)

Rust library for uploading events to AWS CloudWatch Logs.

This library uses [rusoto](https://github.com/rusoto/rusoto). It
handles batching events and various API limits.

See [examples/upload.rs](examples/upload.rs) for a complete example of
how the library can be used to upload batches of events from multiple
threads.
