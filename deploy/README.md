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

The x86_64 image needs an x86-64-v3 CPU (AVX2) because of the prebuilt ONNX
Runtime used by fastembed. The image provides its own glibc runtime.

For opt-in Docker-free operation, install the release binaries and run
`memento setup --daemon native`. Native mode uses SQLite and the Candle
embedder, downloads the model during setup, and starts on demand for CLI, MCP,
and OpenCode plugin requests. It exits after 30 minutes without requests by
default. Its data is separate from the Docker Qdrant volume; stop writes to
Qdrant and the native daemon before using `mementod migrate` to copy it.
