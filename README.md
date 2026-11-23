# buildsystem

An experimental build system for ROS 2: one system for both continuous
integration and package builds.

**This is a test project. It is not an official ROS project, and nothing here
is connected to the ROS build farm or to any official ROS infrastructure.**

## Rules for the code

- Written in Rust.
- No third-party crates, with one exception: `rustls` for TLS, and `ring`, the
  cryptography provider it is built with, for hashing.
- No shelling out to other programs, and no FFI, where it can be avoided.
  Docker and Podman are reached through their HTTP API.

## What is here

| Crate | What it is |
|---|---|
| `crates/crypto` | Hashing and random numbers (from `ring`), base64 and hex |
| `crates/json` | JSON parser and serializer |
| `crates/tar` | tar archive reader and writer |
| `crates/http` | HTTP/1.1 client over any byte stream |
| `crates/container` | Client for the Docker and Podman HTTP API, over a Unix socket or a Windows named pipe |
| `crates/postgres` | PostgreSQL client: wire protocol, SCRAM-SHA-256 login, parameterised queries |
| `crates/agent` | The worker agent, `ci-agent` |

So far the agent has one command, which runs a single job in a container:

```
ci-agent run-job --job job.json --work-dir work
```

It leaves `log.txt`, `outcome.json` and the job's outputs (under `out/`) in the
work directory.

### The job description

```json
{
  "image": "docker.io/library/alpine:3.20",
  "command": ["sh", "-c", "cat /job/in/hello.txt > /job/out/copy.txt"],
  "environment": {"NAME": "value"},
  "parameters": {"anything": "the job needs"},
  "inputs": [{"name": "hello.txt", "url": "file:///tmp/hello.txt"}],
  "timeout_seconds": 600
}
```

### The contract between the agent and a job

- **Job directory.** `/job` on Linux and `C:\job` on Windows, also given in the
  `CI_JOB_DIR` environment variable.
- **Parameters.** `params.json` in the job directory.
- **Inputs.** Files in the `in` subdirectory.
- **Outputs.** Whatever the job writes to the `out` subdirectory is copied out
  when it finishes, whether it passed or failed.
- **Exit code.** Zero is success. 75 means an infrastructure problem that was
  not the fault of the code being built. Anything else is a build failure.

Files go in and out through the container runtime's API. Nothing is mounted
from the host, so the agent can itself run in a container.

## Testing

The tests are defined by `ci/Dockerfile.test` and run the same way anywhere
there is a container runtime:

```
docker build -f ci/Dockerfile.test -t buildsystem-test .
docker run --rm buildsystem-test
```

Some tests need a service, and are skipped unless an environment variable says
where it is:

| Variable | Service | Tests |
|---|---|---|
| `CI_CONTAINER_RUNTIME` | A container runtime's socket | The agent running real jobs |
| `CI_POSTGRES_URL` | A PostgreSQL server | The database client |

The comment at the top of `ci/Dockerfile.test` shows how to supply both.

Without a container, the same checks are:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### What has been tested where

| Runtime | Status |
|---|---|
| Podman on Linux | Tested |
| Docker on Linux | Run by the GitHub workflow |
| Docker on Windows 11, Windows containers with process isolation | Tested |
| PostgreSQL 17 | Tested |

On Windows there is no test Dockerfile yet. The tests run on the host:

```
$env:CI_CONTAINER_RUNTIME = "npipe:////./pipe/docker_engine"
cargo test --workspace
```

## License

Apache-2.0. See [LICENSE](LICENSE).
