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
| `crates/http` | HTTP/1.1 client and server over any byte stream |
| `crates/net` | URLs, TCP connections and TLS (the only crate that uses rustls) |
| `crates/container` | Client for the Docker and Podman HTTP API, over a Unix socket or a Windows named pipe |
| `crates/postgres` | PostgreSQL client: wire protocol, SCRAM-SHA-256 login, parameterised queries |
| `crates/protocol` | What the coordinator and its workers say to each other |
| `crates/coordinator` | The coordinator, `ci-coordinator`: the job queue and the worker and admin APIs |
| `crates/agent` | The worker agent, `ci-agent` |
| `crates/admin` | The administrator's tool, `ci-admin` |
| `crates/e2e` | End-to-end tests of all of the above together |

## Running it

The system is a PostgreSQL database, one coordinator and any number of agents.
Each agent needs a container runtime (Docker or Podman) on its machine.

```
# The coordinator. On its first start it writes the first admin token to
# the file admin-token in the data directory, readable only by its user.
export CI_DATABASE_URL=postgres://user:password@localhost/ros2ci
ci-coordinator --data-dir /var/lib/ros2ci \
    --tls-cert server.pem --tls-key server.key

# The admin tool needs an admin token.
export CI_ADMIN_TOKEN_FILE=/var/lib/ros2ci/admin-token

# A token for a worker. The pool is fixed by the token.
ci-admin create-worker-token --name "builder 1" --pool release > token

# An agent, on any machine that can reach the coordinator.
ci-agent work --coordinator https://coordinator.example:8443 \
    --token-file token --ca-file ca.pem --work-dir /var/lib/ros2ci-agent

# A job.
ci-admin submit --pool release --os linux --arch amd64 --job job.json --wait
```

Without `--tls-cert` and `--tls-key` the worker API is plain HTTP, which is
only for running everything on one machine.

Every call to the admin API needs an admin token, and the coordinator's log
names the token behind each action. `ci-admin create-admin-token` issues more,
so each person or service can have its own, and `ci-admin revoke-admin-token`
takes one away. The admin API also listens on its own address, `127.0.0.1` by
default; keep it private as a second line of defence. If every admin token is
lost or revoked, revoke them all in the database and restart the coordinator:
it writes a new first token when no usable one exists.

How it behaves:

- **Workers connect out.** The coordinator never opens a connection to a
  worker. Workers ask for work every few seconds.
- **Leases.** A job belongs to its worker only while the worker keeps
  reporting. If the reports stop, the job is given to another worker as a new
  attempt, and anything the first worker says afterwards is refused.
- **Retries.** A failure outside the job (a lost worker, an image that will
  not pull, an input that will not download) is retried with a growing delay,
  up to a limit. A failure of the job itself is final.
- **Restarts.** The coordinator keeps nothing in memory. Stopping and starting
  it delays jobs but loses nothing, and running jobs carry on.
- **One coordinator.** A second coordinator pointed at the same database
  refuses to start.

An agent can also run a single job with no coordinator:

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
  "inputs": [{"name": "hello.txt", "url": "https://example.com/hello.txt"}],
  "timeout_seconds": 600
}
```

### The contract between the agent and a job

- **Job directory.** `/job` on Linux and `C:\job` on Windows, also given in the
  `CI_JOB_DIR` environment variable.
- **Parameters.** `params.json` in the job directory.
- **Inputs.** Files in the `in` subdirectory. The agent downloads them from
  `http://` or `https://` URLs. `file://` URLs are accepted only by `run-job`.
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
| `CI_POSTGRES_URL` | A PostgreSQL server whose user may create databases | The database client and the coordinator |
| Both | | The end-to-end tests |

The comment at the top of `ci/Dockerfile.test` shows how to supply both.

Without a container, the same checks are:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

The build comes first because the end-to-end tests run the built programs.

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
$env:CI_POSTGRES_URL = "postgres://user:password@host:5432/postgres"
cargo build --workspace
cargo test --workspace
```

## License

Apache-2.0. See [LICENSE](LICENSE).
