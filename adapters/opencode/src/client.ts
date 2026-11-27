import { spawn } from "node:child_process";
import { isAbsolute } from "node:path";
import { recordActivity, type ActivitySink } from "./activity-log";

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
    constructor(message: string, public code: string, public status?: number, public connectionFailure = false) {
        super(message);
        this.name = "MementoError";
    }
}

function isConnectionFailure(error: unknown): boolean {
    const codes = new Set(["ECONNREFUSED", "EHOSTUNREACH", "ENETUNREACH", "EADDRNOTAVAIL", "ECONNRESET"]);
    let current = error;
    for (let depth = 0; depth < 4 && current && typeof current === "object"; depth++) {
        if ("code" in current && typeof current.code === "string" && codes.has(current.code)) {
            return true;
        }
        current = "cause" in current ? current.cause : undefined;
    }
    return false;
}

export class MementoClient {
    private readonly url: string;
    private readonly token?: string;
    private readonly timeoutMs: number;
    private readonly fetcher: typeof fetch;
    private readonly mementoBin?: string;
    private readonly activity?: ActivitySink;
    private readonly project?: string;
    private readonly startDaemon: (binary: string, url: string) => void;
    private readonly startupWaitMs: number;
    private readonly startupPollMs: number;
    private readonly startupCooldownMs: number;
    private startup?: Promise<boolean>;
    private startupRetryAt = 0;

    constructor(options: {
        url?: string;
        token?: string;
        timeoutMs?: number;
        fetch?: typeof fetch;
        autostart?: boolean;
        mementoBin?: string;
        activity?: ActivitySink;
        project?: string;
        startDaemon?: (binary: string, url: string) => void;
        startupWaitMs?: number;
        startupPollMs?: number;
        startupCooldownMs?: number;
    } = {}) {
        this.url = (options.url ?? "http://127.0.0.1:7377").replace(/\/+$/, "");
        this.token = options.token;
        this.timeoutMs = options.timeoutMs ?? 10000;
        this.fetcher = options.fetch ?? fetch;
        this.mementoBin = options.autostart && options.mementoBin && isAbsolute(options.mementoBin) ? options.mementoBin : undefined;
        this.activity = options.activity;
        this.project = options.project;
        this.startDaemon = options.startDaemon ?? ((binary, url) => {
            const child = spawn(binary, ["daemon", "ensure", "--url", url], { detached: true, stdio: "ignore" });
            child.on("error", () => {});
            child.unref();
        });
        this.startupWaitMs = options.startupWaitMs ?? 15000;
        this.startupPollMs = options.startupPollMs ?? 250;
        this.startupCooldownMs = options.startupCooldownMs ?? 60000;
    }

    private async requestOnce<T>(path: string, method: string, body?: unknown, signal?: AbortSignal): Promise<T> {
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
            throw new MementoError(message, "unavailable", undefined, isConnectionFailure(error));
        }
    }

    private ensureStarted(): Promise<boolean> {
        if (this.startup) {
            return this.startup;
        }
        if (!this.mementoBin || Date.now() < this.startupRetryAt) {
            return Promise.resolve(false);
        }
        const binary = this.mementoBin;
        recordActivity(this.activity, { level: "info", event: "autostart", project: this.project, detail: { outcome: "attempt" } });
        const startup = (async () => {
            try {
                this.startDaemon(binary, this.url);
                const deadline = Date.now() + this.startupWaitMs;
                while (Date.now() < deadline) {
                    try {
                        const health = await this.requestOnce<{ ok: boolean }>("/health", "GET", undefined, AbortSignal.timeout(Math.max(1, deadline - Date.now())));
                        if (health.ok) {
                            return true;
                        }
                    } catch {}
                    const remaining = deadline - Date.now();
                    if (remaining > 0) {
                        await new Promise<void>((resolve) => setTimeout(resolve, Math.min(this.startupPollMs, remaining)));
                    }
                }
            } catch {
                return false;
            }
            return false;
        })().then((success) => {
            if (!success) {
                this.startupRetryAt = Date.now() + this.startupCooldownMs;
            }
            recordActivity(this.activity, { level: success ? "info" : "warn", event: "autostart", project: this.project, detail: { outcome: success ? "ready" : "failed" } });
            return success;
        }).finally(() => {
            if (this.startup === startup) {
                this.startup = undefined;
            }
        });
        this.startup = startup;
        return startup;
    }

    private async request<T>(path: string, method: string, body?: unknown, signal?: AbortSignal, waitForStartup = true): Promise<T> {
        try {
            return await this.requestOnce<T>(path, method, body, signal);
        } catch (error) {
            if (!(error instanceof MementoError) || !error.connectionFailure || signal?.aborted || !this.mementoBin) {
                throw error;
            }
            const startup = this.ensureStarted();
            if (!waitForStartup) {
                void startup;
                throw error;
            }
            const ready = signal
                ? await new Promise<boolean>((resolve) => {
                    const abort = () => {
                        signal.removeEventListener("abort", abort);
                        resolve(false);
                    };
                    signal.addEventListener("abort", abort, { once: true });
                    if (signal.aborted) {
                        abort();
                    }
                    void startup.then((result) => {
                        signal.removeEventListener("abort", abort);
                        resolve(result);
                    });
                })
                : await startup;
            if (!ready || signal?.aborted) {
                throw error;
            }
            return this.requestOnce<T>(path, method, body, signal);
        }
    }

    health(): Promise<{ ok: boolean; qdrant: boolean }> {
        return this.request("/health", "GET");
    }

    putSession(payload: SessionRecord, waitForStartup = true): Promise<unknown> {
        return this.request("/v1/sessions", "PUT", payload, undefined, waitForStartup);
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

    claimContinuation(payload: SessionIdentity, waitForStartup = true): Promise<ClaimResponse> {
        return this.request("/v1/continuations/claim", "POST", payload, undefined, waitForStartup);
    }

    putContinuation(payload: { project: string; parent_harness: string; parent_session: string; target_harness?: string }): Promise<unknown> {
        return this.request("/v1/continuations", "PUT", payload);
    }

    lineage(harness: string, session: string): Promise<LineageResponse> {
        return this.request(`/v1/sessions/${encodeURIComponent(harness)}/${encodeURIComponent(session)}/lineage`, "GET");
    }

    ingest(payload: SessionIdentity & { items: IngestItem[] }, waitForStartup = true): Promise<{ segments: number; skipped: number; duplicates?: number }> {
        return this.request("/v1/ingest", "POST", payload, undefined, waitForStartup);
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
