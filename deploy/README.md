# Deployment

From the repository root:

```sh
docker compose -f deploy/compose.yaml up -d --build
docker compose -f deploy/compose.yaml logs -f
docker compose -f deploy/compose.yaml down
```

Qdrant data persists in the `qdrant_storage` volume. The daemon listens on
`127.0.0.1:7377`; Qdrant is not exposed on the host. The image build downloads
the embedding model. Set `MEMENTO_TOKEN` in the environment to pass it to the
daemon. This milestone exposes only the unauthenticated health endpoint.
