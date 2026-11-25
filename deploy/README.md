# Running the system in containers

The smallest complete installation is three containers on one machine:
PostgreSQL, the coordinator and one worker. Workers on other machines can
then be added, in a container or not.

What you need: Docker or Podman, a compose tool (`docker compose`,
`docker-compose` or `podman compose`), and `openssl` to make certificates.

| File | What it is |
|---|---|
| `Containerfile` | Builds two images, `ros2ci-coordinator` and `ros2ci-agent` |
| `compose.yaml` | The three containers |
| `make-certs.sh` | Makes a private certificate authority and the coordinator's certificate |
| `env.example` | The settings, to copy to `.env` |

## Steps

Run these in this directory. With Podman, point the compose tool at
Podman's socket first, for example
`export DOCKER_HOST=unix:///run/user/$(id -u)/podman/podman.sock`.

1. **Certificates.** Name every host name and address that workers will
   use to reach the coordinator:

   ```
   ./make-certs.sh ci.example.org 192.0.2.10
   ```

2. **Settings.** Copy `env.example` to `.env`, and set the database
   password and the path of the container runtime's socket. Keep the file
   private: `chmod 600 .env`.

3. **PostgreSQL and the coordinator.**

   ```
   docker compose up -d postgres coordinator
   ```

   On its first start the coordinator creates the database tables and the
   first admin token.

4. **A token for the worker.** The pool is fixed by the token.

   ```
   mkdir -p secrets
   docker compose exec -T coordinator \
       ci-admin create-worker-token --name "first worker" --pool default \
       > secrets/worker-token
   chmod 644 secrets/worker-token
   chmod 700 secrets
   ```

5. **The worker.**

   ```
   docker compose --profile worker up -d worker
   docker compose exec -T coordinator ci-admin list-workers
   ```

## Using it

The admin tool is in the coordinator's container:

```
docker compose exec -T coordinator ci-admin list-workers
docker compose exec -T coordinator ci-admin status 1
```

The admin API is also published on this machine's loopback address, port
8444, for an admin tool outside the container. That tool needs an admin
token; the first one is in the coordinator's data volume:

```
docker compose exec -T coordinator cat /data/admin-token > admin-token
chmod 600 admin-token
export CI_ADMIN_TOKEN_FILE=$PWD/admin-token
ci-admin submit --pool default --os linux --arch amd64 --job job.json --wait
```

From another machine, reach that port through SSH and nothing else:
`ssh -L 8444:127.0.0.1:8444 <this machine>`.

## Workers on other machines

A worker needs three things: to reach the coordinator's worker port
(8443), the file `certs/ca.pem`, and a token of its own.

```
docker compose exec -T coordinator \
    ci-admin create-worker-token --name "builder 2" --pool default > token
```

Then, on the other machine, either run the agent directly:

```
ci-agent work --coordinator https://ci.example.org:8443 \
    --token-file token --ca-file ca.pem --work-dir /var/lib/ros2ci-agent
```

or in a container, with the runtime's socket mounted in:

```
docker run -d --init --name ros2ci-worker --security-opt label=disable \
    -v /var/run/docker.sock:/run/ros2ci/runtime.sock \
    -v $PWD/token:/run/ros2ci/worker-token:ro \
    -v $PWD/ca.pem:/run/ros2ci/ca.pem:ro \
    -v ros2ci-worker:/work \
    ros2ci-agent work --coordinator https://ci.example.org:8443 \
        --token-file /run/ros2ci/worker-token --ca-file /run/ros2ci/ca.pem \
        --work-dir /work --runtime unix:///run/ros2ci/runtime.sock
```

On Windows the agent runs directly, not in a container.

## Things to know

- **Job containers are siblings of the worker's container,** not children.
  The worker starts them through the runtime's socket, so they and the
  job images live in the host's container storage.
- **The worker's container can control the container runtime.** Mounting
  the socket gives it everything that runtime can do on the host. With
  rootless Podman that is the user's own account; with Docker it is root.
- **The worker runs as root inside its container,** because that is who
  may use the socket. With rootless Podman, root in the container is the
  user who owns the socket. The coordinator runs as its own user.
- **Stopping.** Neither program handles signals, so both run under an init
  process (`init: true`, or `--init`) that makes a stop take effect at
  once. Stopping a worker in the middle of a job makes the coordinator
  run the job again elsewhere; to avoid that, drain the worker first
  (`ci-admin drain-worker <id>`) and it exits when its job is done.
- **What to back up.** The `postgres` volume and the `coordinator` volume
  (logs, artifacts, stored files and the first admin token), and
  `certs/ca.key`.
- **Renewing the certificate.** Run `make-certs.sh` again with the same
  names. The authority is kept, so workers need no change. Then restart
  the coordinator.
- **Private files.** `.env`, `certs/` and `secrets/` are not for sharing
  and are ignored by git. The files that containers read are readable by
  everyone on the machine by their mode; the directories around them
  (`chmod 700`) are what keep other users out.
