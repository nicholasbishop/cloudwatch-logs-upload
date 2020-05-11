# cloudwatch-logs-upload

Rust library for uploading events to AWS CloudWatch Logs.

This library uses [rusoto](https://github.com/rusoto/rusoto). It
handles batching events and various API limits.

See [examples/upload.rs](examples/upload.rs) for a complete example of
how the library can be used from multiple threads.
