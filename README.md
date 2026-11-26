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
| `crates/scale` | The scale test's tool, `ci-scale`: a made-up ROS distribution and stand-in workers whose jobs finish at once |

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
- **The index is written after the change, not as part of it.** A
  publish puts the files in place and records the build; the repository
  is then marked, and its index is written from the database: at once by
  `ci-admin publish`, and once a step by the reconciler, for however many
  builds it published. Writing an index costs as much as the repository
  is large. The files a build replaced are removed after the new index
  is in place. A coordinator that stopped in between writes the index
  when it starts.
- **Old index files.** The parts of the index are named by their hash, so
  a client that read the top a moment before a change still finds what
  it names. Superseded parts are kept for an hour, and at most twelve of
  them in a directory, the newest; a large rebuild would otherwise leave
  a copy of the whole index for each of its steps.

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

### The reconciler

Given settings, the coordinator follows a rosdistro and keeps the
repositories in step with it, with nobody running a command:

```
ci-admin reconcile-config home-lab.json
ci-admin reconcile-status
ci-admin rebuild --package rcutils
```

- **Intake.** It asks the rosdistro's git server which commit the branch
  is at. For a new commit it runs the release check as a job, and then a
  source archive job for each new release. Builds go on from the last
  commit that was taken in whole; a commit whose check fails changes
  nothing and is shown with its problems.
- **Order and rebuilds come from one rule.** A package is up to date when
  what is published was built from the release's source against the
  builds of its dependencies that are published now. So a package waits
  for its dependencies, and when a dependency is published again its
  rebuild counter changes and the package is built again.
- **States.** Each package of each target is up to date, queued,
  building, blocked or failed. A failed build is tried again a set number
  of times, with a delay that doubles; after that it stays failed until
  its source or a dependency changes, or someone asks for a rebuild.
- **A step publishes, then writes the indexes, then queues.** Every
  build that finished since the last step is published, on every target;
  then the index of each repository that changed is written, once; then
  builds are queued. So a build is queued only when the index lists what
  it depends on. A line in the log says what each step did and how long
  each part took, and `reconcile-status --json` has the time of the last
  step.
- **Nothing is kept in memory.** Every step reads where things stand from
  the database and writes down what it did. A restart, or a worker that
  dies with a job, delays builds and loses none.
- **Tests per target.** A target's settings say whether its builds run
  the packages' tests. Without tests, a package does not wait for what
  only its tests depend on.
- **Not done:** removing from a repository a package that is no longer
  released; rebuilding when the job image or the job script changes.

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

### The scale test

`ci-scale` makes up a ROS distribution and stands in for everything
outside the coordinator: the rosdistro's git server, and workers that
answer each job at once with what it would have left behind. What is left
to measure is the coordinator, the reconciler and the database.

```
ci-scale generate --packages 2000 --output distribution.json
ci-scale run --distribution distribution.json --coordinator http://127.0.0.1:8443 \
    --git-listen 127.0.0.1:8446 \
    --workers linux:amd64:2:pull-request-token --workers linux:amd64:60:release-token \
    --workers linux:arm64:40:release-token
ci-admin reconcile-config settings.json        # the full build starts
ci-scale bump distribution.json --package pkg_00000 --rebuilt expected.txt
                                               # a release of the lowest package
```

The generated distribution has the shape of Rolling as its rosdistro
cache had it on 2026-11-26 (1,600 packages, 642 repositories, 6.5
dependencies a package, a longest dependency chain of 42, 91% of the
packages standing on the lowest one), scaled to the number of packages
asked for. The reconciler's settings name the git server as the
rosdistro's repository, and each target's job carries a `scale`
parameter that says what to pretend to build;
`crates/scale/example-settings.json` is the settings the measurements
below were made with, for repositories named `scale-resolute`,
`scale-trixie` and `scale-el10`.

Measured on 2026-11-26 with 2,000 packages in 690 repositories on 5
targets (three apt, two RPM, in three repositories), 100 stand-in
workers, and the coordinator and PostgreSQL 17 together on one machine
with 4 cores and 8 GB of memory:

| | Full build from nothing | New version of the lowest package |
|---|---|---|
| Builds | 10,000 | 9,605, which is every build that depends on it and no other |
| Time | 9 min 45 s | 9 min 30 s |
| Coordinator | 88 s of processor time, 189 MB at its largest | 90 s, 199 MB |
| PostgreSQL | 107 s of processor time, 396,000 transactions | 106 s, 368,000 transactions |
| Database size | 9 MB to 245 MB | 246 MB to 442 MB |
| Longest step of the reconciler | 32 s, to queue 960 builds | 33 s |

In both, every build was queued only after the builds it depends on
were published and listed in the index. The time is the reconciler's:
the dependency chain is 42 packages long, each link needs a step, and a
step that queues hundreds of builds waits its turn at the database
behind the stand-in workers, which all answer at once as real ones do
not. With nothing to do, a step takes 0.2 s.

What the database grows by is the jobs: each build job carries the list
of the distribution's 690 releases, about 110 kB, which the database
stores in about 16 kB. Nothing removes old jobs yet.

About 2% of the stand-in workers' calls found the coordinator's
connection queue full, were closed, and were made again. None was lost.

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
