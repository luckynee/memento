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

The image needs a glibc x86-64-v3 CPU (AVX2) because of the prebuilt ONNX
Runtime used by fastembed. The local Docker install uses the legacy builder;
installing buildx is optional.
