export type IngestItem = {
    source_id: string;
    ts: string;
    role: "user" | "assistant" | "tool";
    text: string;
    kind_hint: string | null;
    tool: { name: string; input: unknown; output: string; ok: boolean } | null;
    injected?: boolean;
};

export type SessionIdentity = {
    harness: string;
    project: string;
    session: string;
};

export type SessionRecord = SessionIdentity & {
    parent?: string;
    parent_harness?: string;
    parent_cutoff?: string;
    require_parent?: boolean;
    retention?: string;
    title?: string;
    last_active_at?: string | null;
    deleted_at?: string | null;
};

export type LineageResponse = {
    lineage: { harness: string; session: string }[];
};

export type ClaimResponse = {
    claimed: boolean;
    parent_harness?: string;
    parent?: string;
    digest?: string;
};

export type SearchScope = "session" | "lineage" | "project" | "global";

export type SearchHit = {
    id: string;
    score: number;
    kind: string;
    text: string;
    harness?: string;
    session: string;
    ts: string;
    path?: string;
};

export class MementoError extends Error {
    constructor(message: string, public code: string, public status?: number) {
        super(message);
        this.name = "MementoError";
    }
}

export class MementoClient {
    private readonly url: string;
    private readonly token?: string;
    private readonly timeoutMs: number;
    private readonly fetcher: typeof fetch;

    constructor(options: { url?: string; token?: string; timeoutMs?: number; fetch?: typeof fetch } = {}) {
        this.url = (options.url ?? "http://127.0.0.1:7377").replace(/\/+$/, "");
        this.token = options.token;
        this.timeoutMs = options.timeoutMs ?? 10000;
        this.fetcher = options.fetch ?? fetch;
    }

    private async request<T>(path: string, method: string, body?: unknown, signal?: AbortSignal): Promise<T> {
        const timeout = AbortSignal.timeout(this.timeoutMs);
        const combined = signal ? AbortSignal.any([timeout, signal]) : timeout;
        const headers: Record<string, string> = { "Content-Type": "application/json" };
        if (this.token) {
            headers.Authorization = `Bearer ${this.token}`;
        }
        try {
            const response = await this.fetcher(`${this.url}${path}`, {
                method,
                headers,
                body: body === undefined ? undefined : JSON.stringify(body),
                signal: combined,
            });
            if (!response.ok) {
                let error: unknown;
                try {
                    error = await response.json();
                } catch {
                    error = undefined;
                }
                const detail = error && typeof error === "object" && "error" in error
                    ? error.error
                    : undefined;
                if (detail && typeof detail === "object" && "code" in detail && "message" in detail
                    && typeof detail.code === "string" && typeof detail.message === "string") {
                    throw new MementoError(detail.message, detail.code, response.status);
                }
                throw new MementoError(`HTTP ${response.status}`, `http_${response.status}`, response.status);
            }
            return await response.json() as T;
        } catch (error) {
            if (error instanceof MementoError) {
                throw error;
            }
            const message = error instanceof Error ? error.message : String(error);
            if (combined.aborted) {
                throw new MementoError(message, "timeout");
            }
            throw new MementoError(message, "unavailable");
        }
    }

    health(): Promise<{ ok: boolean; qdrant: boolean }> {
        return this.request("/health", "GET");
    }

    putSession(payload: SessionRecord): Promise<unknown> {
        return this.request("/v1/sessions", "PUT", payload);
    }

    listSessions(harness: string | undefined, project: string, signal?: AbortSignal): Promise<{ sessions: SessionRecord[] }> {
        const query = new URLSearchParams();
        if (harness !== undefined) {
            query.set("harness", harness);
        }
        query.set("project", project);
        return this.request(`/v1/sessions?${query}`, "GET", undefined, signal);
    }

    stats(identity: SessionIdentity, signal?: AbortSignal): Promise<{ by_kind: Record<string, number> }> {
        const query = new URLSearchParams(identity);
        return this.request(`/v1/stats?${query}`, "GET", undefined, signal);
    }

    releaseSession(payload: SessionIdentity & { reason?: string }): Promise<{ released: boolean; removed: { harness: string; session: string }[] }> {
        return this.request("/v1/sessions/release", "POST", payload);
    }

    claimContinuation(payload: SessionIdentity): Promise<ClaimResponse> {
        return this.request("/v1/continuations/claim", "POST", payload);
    }

    putContinuation(payload: { project: string; parent_harness: string; parent_session: string; target_harness?: string }): Promise<unknown> {
        return this.request("/v1/continuations", "PUT", payload);
    }

    lineage(harness: string, session: string): Promise<LineageResponse> {
        return this.request(`/v1/sessions/${encodeURIComponent(harness)}/${encodeURIComponent(session)}/lineage`, "GET");
    }

    ingest(payload: SessionIdentity & { items: IngestItem[] }): Promise<{ segments: number; skipped: number; duplicates?: number }> {
        return this.request("/v1/ingest", "POST", payload);
    }

    search(payload: SessionIdentity & { query: string; scope?: SearchScope; k?: number; kinds?: string[] }, signal?: AbortSignal): Promise<{ hits: SearchHit[] }> {
        return this.request("/v1/search", "POST", payload, signal);
    }

    digest(payload: SessionIdentity & { budget: number; profile?: "default" | "subagent" }, signal?: AbortSignal): Promise<{ text: string; segments: number }> {
        const query = new URLSearchParams({
            harness: payload.harness,
            project: payload.project,
            session: payload.session,
            budget: String(payload.budget),
        });
        if (payload.profile !== undefined) {
            query.set("profile", payload.profile);
        }
        return this.request(`/v1/digest?${query}`, "GET", undefined, signal);
    }
}
