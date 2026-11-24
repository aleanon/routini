# routini

See the [README at the repository root](../README.md). Tests need
[nextest](https://nexte.st) (`cargo nextest run`): every test runs in its own process, because
the proxy keeps process-wide state such as metrics.
