# Deployment

From the repository root:

```sh
docker compose -f deploy/compose.yaml up -d --build
docker compose -f deploy/compose.yaml logs -f
docker compose -f deploy/compose.yaml down
```

Qdrant data persists in the `qdrant_storage` volume. The daemon listens on
`127.0.0.1:7377`; Qdrant is not exposed on the host. The image build downloads
the embedding model, so containers start offline. Set `MEMENTO_TOKEN` in the
environment to require `Authorization: Bearer <token>` on every route except
`/health`. Both services use `restart: unless-stopped`, so they come back after
a reboot until you run `down`.

The image embeds with the Candle CPU backend and has no AVX2 requirement. The
image provides its own glibc runtime.

Native mode is the default for new setups. For Docker-free operation, install the release binaries and run
`memento setup --daemon native`. Native mode uses SQLite and the Candle
embedder, downloads the model during setup, and starts on demand for CLI, MCP,
and OpenCode plugin requests. It exits after 30 minutes without requests by
default. Its data is separate from the Docker Qdrant volume; run
`memento daemon migrate` to copy Docker memories into it.
