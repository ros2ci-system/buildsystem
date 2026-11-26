# buildsystem

An experimental build system for ROS 2: one system for both continuous
integration and package builds.

**This is a test project. It is not an official ROS project, and nothing here
is connected to the ROS build farm or to any official ROS infrastructure.**

## Rules for the code

- Written in Rust.
- No third-party crates, with one exception: `rustls` for TLS, with `ring`, the
  cryptography provider it is built with, also used for hashing, and
  `webpki-roots`, the public certificate authorities as data.
- No shelling out to other programs, and no FFI, where it can be avoided.
  Docker and Podman are reached through their HTTP API.

## What is here

| Crate | What it is |
|---|---|
| `crates/crypto` | Hashing, Ed25519 signatures and random numbers (from `ring`), base64 and hex |
| `crates/openpgp` | OpenPGP signatures over repository indexes: the public key, detached and cleartext signatures |
| `crates/json` | JSON parser and serializer |
| `crates/tar` | tar archive reader and writer |
| `crates/http` | HTTP/1.1 client and server over any byte stream |
| `crates/net` | URLs, TCP connections and TLS (the only crate that uses rustls) |
| `crates/container` | Client for the Docker and Podman HTTP API, over a Unix socket or a Windows named pipe |
| `crates/postgres` | PostgreSQL client: wire protocol, SCRAM-SHA-256 login, parameterised queries |
| `crates/protocol` | What the coordinator and its workers say to each other |
| `crates/coordinator` | The coordinator, `ci-coordinator`: the job queue, the worker and admin APIs, and the package repositories |
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

To run all of this in containers, see [deploy/README.md](deploy/README.md).

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
- **Draining.** `ci-admin drain-worker <worker-id>` stops a worker cleanly:
  it finishes the job it is running, is given no more, and its agent exits
  with status 0. `ci-admin list-workers` shows the worker ids. An agent that
  is started again registers as a new worker and takes jobs as usual; to keep
  a machine out for good, revoke its token.

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
  "timeout_seconds": 600,
  "shared_memory_mb": 2048
}
```

`shared_memory_mb` is optional. It is the size of `/dev/shm` in a Linux
container, which is 64 MiB without it. That is too little for some of what
gets tested: shared-memory transports, and tracing buffers, which are per
CPU and so grow with the worker. Windows containers have no `/dev/shm`, and
the member is ignored there.

### Images

A job names its image in one of two ways.

A **reference** is something a registry can supply:

```json
"image": "registry.example/core@sha256:0123..."
```

A **build definition** is the recipe itself: a build context (a tar archive
holding the build file and what it copies in) and the build file's path in it.

```json
"image": {"build": {"context": "stored:ubuntu-noble.tar", "build_file": "Dockerfile"}}
```

A worker that has not built that recipe yet builds it and keeps the result, so
no registry is needed. The image is kept under a name made from the context's
hash, the build file's path and a `generation` number, so the same recipe is
built once per worker and a changed recipe is a new image. Raising
`generation` forces a rebuild of an unchanged recipe.

An **image build job** builds the image and does nothing else:

```json
{
  "kind": "image_build",
  "image": {"build": {"context": "stored:ubuntu-noble.tar", "build_file": "Dockerfile"}},
  "timeout_seconds": 3600
}
```

If the build file fails, an image build job is a build failure. A job that
only needed the image to run in counts it as an infrastructure failure and is
retried.

### Stored files

An administrator can store files on the coordinator, and jobs read them as
`stored:<name>`, as an input or as a build context:

```
ci-admin store-context ubuntu-noble.tar images/ubuntu-noble
ci-admin store-file license.dat /path/to/license.dat
```

Only the worker running a job that names a stored file can download it, and
only while it runs that job. That is what keeps a private file, such as a
license, to the jobs it was given to. A job reads whatever is stored when it
runs, so replacing a file takes effect at once.

### The contract between the agent and a job

- **Job directory.** `/job` on Linux and `C:\job` on Windows, also given in the
  `CI_JOB_DIR` environment variable.
- **Parameters.** `params.json` in the job directory.
- **Inputs.** Files in the `in` subdirectory. The agent downloads them from
  `http://` or `https://` URLs, or from the coordinator for `stored:` names.
  `file://` URLs are accepted only by `run-job`.
- **Outputs.** Whatever the job writes to the `out` subdirectory is copied out
  when it finishes, whether it passed or failed.
- **Exit code.** Zero is success. 75 means an infrastructure problem that was
  not the fault of the code being built. Anything else is a build failure.

Files go in and out through the container runtime's API. Nothing is mounted
from the host, so the agent can itself run in a container.

### Package repositories

The coordinator keeps apt and RPM repositories under `repos` in its data
directory and writes and signs their indexes itself. The key is made
with `ci-admin generate-signing-key` and given to the coordinator as
`--signing-key`; it never reaches a worker. `--repo-listen` serves the
repositories read-only.

Publishing is a step of its own, after a build:

```
ci-admin create-repository --name rolling-resolute --format apt --suite resolute \
    --version-suffix resolute --architectures amd64,arm64 --source-arch amd64
ci-admin next-revision --repository rolling-resolute --package ament_package --version 0.19.1
ci-admin submit --pool release --os linux --arch amd64 --job job.json --wait
ci-admin publish --repository rolling-resolute --job 7 --package ament_package \
    --version 0.19.1 --revision 1 --name ros-rolling-ament-package
```

- **The rebuild counter.** `next-revision` hands out 1 for the first build
  of a version of a package and one more each time after. The build is
  given it as its revision, and the package's version ends in it
  (`0.19.1-1resolute`, `0.19.1-1.el10`). A counter is never handed out
  twice, so a rebuild always sorts above what it replaces.
- **What a job says is checked.** A package build describes its packages
  in `repo-metadata.json`, in the words of the distribution's own tools.
  A publish names the packages the build was for. A package with another
  name, another version or another architecture is refused, and every
  path, size and hash in the index is the coordinator's own.
- **What is signed.** The top of each index (`InRelease` and
  `Release.gpg` for apt, `repomd.xml.asc` for RPM), which names the files
  under it by hash. Packages are not signed one by one. This is how the
  ROS repositories are set up, and clients are configured the same way
  (for dnf: `repo_gpgcheck=1`, `gpgcheck=0`).
- **Nothing is compressed.** There is no compressor in this code. apt
  and dnf both read the plain index files.

### Releases and the source archive

A release enters the system through two jobs, which `ci-admin release` runs
for a rosdistro commit: a release check, which builds nothing, and one
source archive job for each release the archive does not hold yet.

```
ci-admin release --rosdistro-url https://raw.githubusercontent.com/example/rosdistro \
    --commit 5a071f7f --check-job release-check.json --archive-job source-archive.json
ci-admin archived
```

- **The archive.** The coordinator keeps each release's tarball under
  `archive` in its data directory, named by its SHA-256, with a record of
  the repository, tag, commit and packages. An entry is never replaced.
- **Reading it.** A job names a tarball as the input `archive:<sha256>`.
  The worker downloads it from the coordinator and checks the hash. A job
  that names a tarball the archive does not hold is refused when it is
  submitted.
- **Moved tags.** The check looks at the tag of every archived release
  again. A tag that names another commit than the archived one, or is
  gone, is reported, and builds go on reading the archive.
- **Where the logic is.** The check and the archive job are scripts in the
  configuration, and their job descriptions are the two files the command
  is given. The command adds their parameters and tells the coordinator
  to take each tarball in; the coordinator checks the job's own record
  against the release the command names.

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
