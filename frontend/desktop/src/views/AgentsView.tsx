import { CardHeader } from "@agentdesktop/ui";
import { useEffect, useRef, useState } from "react";

import { getCatalogJobs, requestCatalogJob } from "../backend";
import type { CatalogJobRequest, CatalogJobsView } from "../types";

export function jobStateLabel(state: number): string {
  return (
    ["Unknown", "Queued", "Dispatched", "Accepted", "Running",
      "Cancel requested", "Succeeded", "Failed", "Cancelled", "Expired"][state] ??
    "Unknown"
  );
}

export function jobActionLabel(action: number): string {
  return ["Unknown", "Install", "Uninstall", "Update"][action] ?? "Unknown";
}

export function AgentsView() {
  const [view, setView] = useState<CatalogJobsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [versions, setVersions] = useState<Record<string, string>>({});
  const [pending, setPending] = useState<CatalogJobRequest | null>(null);
  const [attempted, setAttempted] = useState(false);
  const [busy, setBusy] = useState(false);
  const requestInFlight = useRef(false);

  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function refresh() {
      try {
        const result = await getCatalogJobs();
        if (active) {
          setView(result);
          setError(null);
        }
      } catch (err) {
        if (active) setError(String(err));
      } finally {
        if (active) timer = setTimeout(refresh, 5000);
      }
    }
    void refresh();
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, []);

  const now = Math.floor(Date.now() / 1000);
  const canRequest = !!view?.selfServiceAllowed && !error &&
    view.expiresAt !== null && view.expiresAt > now && !pending;

  function review(agentId: string, version: string, action: CatalogJobRequest["action"]) {
    if (!canRequest || !view || view.revision === null || !view.sha256 || !view.expiresAt) return;
    setPending({
      agentId, version, action,
      idempotencyKey: crypto.randomUUID(),
      deadlineUnixSeconds: Math.min(now + 1800, view.expiresAt),
      catalogRevision: view.revision,
      catalogSha256: view.sha256,
    });
    setAttempted(false);
    setNotice(null);
  }

  async function submit() {
    if (!pending || requestInFlight.current) return;
    requestInFlight.current = true;
    setBusy(true);
    setAttempted(true);
    setNotice(null);
    try {
      const id = await requestCatalogJob(pending);
      setNotice(`Request accepted: ${id}. Installation is not yet complete.`);
      setPending(null);
      setAttempted(false);
    } catch (err) {
      setNotice(`Request not confirmed: ${String(err)}. Retry the same request or review job history before submitting another.`);
    } finally {
      setBusy(false);
      requestInFlight.current = false;
    }
  }

  return (
    <div className="page-stack">
      <div className="page-heading">
        <div>
          <h2>Managed agent catalog</h2>
          <p>Only signed catalog versions approved by your organization are available.</p>
        </div>
      </div>
      {error ? <div className="notice notice-error" role="alert">{error}</div> : null}
      {notice ? <div className="notice" role="status">{notice}</div> : null}
      <section className="card">
        <CardHeader
          title="Catalog status"
          description={view?.revision == null ? "Waiting for a verified catalog" : `Verified revision ${view.revision}`}
        />
        <p>{view?.localExecutionEnabled ? "Local macOS user installation enabled." : "Local installation disabled or unsupported."}</p>
        <p>{view?.selfServiceAllowed ? "Self-service requests are permitted." : "Self-service requests are not currently permitted."}</p>
        {view?.lastSyncUnixSeconds ? <p>Last verified sync: {new Date(view.lastSyncUnixSeconds * 1000).toLocaleString()}</p> : null}
      </section>
      {pending ? (
        <section className="card">
          <CardHeader title="Review request" description={`${pending.action} ${pending.agentId} ${pending.version}`} />
          <p>This sends a job to your controller for this device. It does not directly execute a command. Only managed files can be removed.</p>
          <button type="button" disabled={busy} onClick={() => void submit()}>
            {busy ? "Submitting…" : attempted ? "Retry same request" : "Confirm request"}
          </button>{" "}
          <button type="button" disabled={busy} onClick={() => {
            if (attempted) setNotice("Request outcome may be unknown. Review job history before creating another request.");
            setPending(null);
            setAttempted(false);
          }}>Close review</button>
        </section>
      ) : null}
      <section className="card">
        <CardHeader title="Approved agents" description="Select a version and review the requested action." />
        {view?.entries.length ? view.entries.map((entry) => {
          const version = versions[entry.id] ?? entry.versions[0] ?? "";
          return (
            <div className="definition-row" key={entry.id}>
              <div>
                <strong>{entry.displayName}</strong>
                <p>{entry.supported ? entry.id : "Unsupported OS or architecture"}</p>
                <label>
                  Version{" "}
                  <select value={version} onChange={(event) => setVersions((old) => ({
                    ...old, [entry.id]: event.target.value,
                  }))}>
                    {entry.versions.map((item) => <option key={item}>{item}</option>)}
                  </select>
                </label>
              </div>
              <div>
                {(["install", "update", "uninstall"] as const).map((action) => (
                  <button key={action} type="button" disabled={!canRequest || !entry.supported || !version}
                    onClick={() => review(entry.id, version, action)}>{action}</button>
                ))}
              </div>
            </div>
          );
        }) : <p>No verified catalog entries.</p>}
      </section>
      <section className="card">
        <CardHeader title="This device's job history" description="Controller-reported state, refreshed every five seconds." />
        {view?.jobs.length ? view.jobs.map((job) => (
          <div className="definition-row" key={job.id}>
            <div><strong>{jobActionLabel(job.action)} {job.agentId} {job.version}</strong>
              <p>{new Date(job.updatedAt * 1000).toLocaleString()}</p></div>
            <div>{jobStateLabel(job.state)}{job.errorCode ? ` · ${job.errorCode}` : ""}</div>
          </div>
        )) : <p>No jobs recorded.</p>}
      </section>
    </div>
  );
}
