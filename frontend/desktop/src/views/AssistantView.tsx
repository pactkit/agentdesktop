import { CardHeader } from "@agentdesktop/ui";
import { useEffect, useState } from "react";

import { askAssistant, getAssistantStatus, getCatalogJobs } from "../backend";
import type {
  AssistantStatus,
  CatalogJobsView,
  ConnectorSnapshot,
  Discovery,
  ManagedDeviceSnapshot,
} from "../types";
import type { View } from "../useDesktopModel";
import { jobStateLabel } from "./AgentsView";

interface Diagnosis {
  title: string;
  detail: string;
  next: View;
}

/** Deterministic, local-only diagnostic guidance. No secrets, arbitrary
 * commands, model tools, file modification, or external prompt submission. */
export function diagnose(
  connector: ConnectorSnapshot | null,
  discovery: Discovery | null,
  enrollment: ManagedDeviceSnapshot | null,
  jobs: CatalogJobsView | null,
): Diagnosis[] {
  const result: Diagnosis[] = [];
  if (connector?.state !== "ready") {
    result.push({
      title: "Local daemon needs attention",
      detail: "Check the daemon status and refresh. Review the organization's installed client before changing any files.",
      next: "home",
    });
  }
  const controller = connector?.runtime?.controller;
  if (controller && !controller.connected) {
    result.push({
      title: "Controller connection unavailable",
      detail: controller.lastError === "identityRejected" || controller.lastError === "sessionExpired"
        ? "Review the enrollment state. Ask the administrator whether this identity was retired or revoked."
        : "Check the organization's network and controller service. Do not disable TLS verification.",
      next: "home",
    });
  }
  if (enrollment?.configured && !discovery) {
    result.push({
      title: "Tool inventory unavailable",
      detail: "Refresh the inventory after checking the daemon. Missing inventory is not proof that a tool is uninstalled.",
      next: "tools",
    });
  }
  if (jobs) {
    if (!jobs.localExecutionEnabled) {
      result.push({
        title: "Managed installation disabled",
        detail: "The endpoint must be a locally opted-in macOS user daemon with pinned catalog keys. Contact the operator.",
        next: "agents",
      });
    }
    if (jobs.expiresAt !== null && jobs.expiresAt <= Math.floor(Date.now() / 1000)) {
      result.push({
        title: "Catalog expired",
        detail: "The controller operator must publish a valid signed catalog. Do not bypass signature verification.",
        next: "agents",
      });
    }
    for (const job of jobs.jobs.filter((job) => job.state === 7 || job.state === 9).slice(0, 5)) {
      const guidance = job.errorCode === "preflight_failed"
        ? "Verify catalog version, OS/architecture, artifact signature, HTTPS reachability and the deadline."
        : job.errorCode === "interrupted" || job.errorCode === "execution_failed"
          ? "The outcome may be ambiguous. Review the managed installation and receipt before retrying; never replay blindly."
          : "Review the job deadline, device identity and managed file ownership. Ask the operator to reconcile the task.";
      result.push({
        title: `${job.agentId} ${job.version}: ${jobStateLabel(job.state)}`,
        detail: guidance,
        next: "agents",
      });
    }
  }
  return result;
}

export function AssistantView({ connector, discovery, enrollment, onNavigate }: {
  connector: ConnectorSnapshot | null;
  discovery: Discovery | null;
  enrollment: ManagedDeviceSnapshot | null;
  onNavigate: (view: View) => void;
}) {
  const [jobs, setJobs] = useState<CatalogJobsView | null>(null);
  const [jobError, setJobError] = useState(false);
  const [assistant, setAssistant] = useState<AssistantStatus | null>(null);
  const [question, setQuestion] = useState("");
  const [answer, setAnswer] = useState<string | null>(null);
  const [askError, setAskError] = useState<string | null>(null);
  const [asking, setAsking] = useState(false);
  useEffect(() => {
    let active = true;
    getCatalogJobs().then(
      (result) => { if (active) setJobs(result); },
      () => { if (active) setJobError(true); },
    );
    getAssistantStatus().then(
      (result) => { if (active) setAssistant(result); },
      () => { if (active) setAssistant(null); },
    );
    return () => { active = false; };
  }, []);
  const findings = diagnose(connector, discovery, enrollment, jobs);
  async function ask() {
    if (asking || !assistant?.enabled || !question.trim()) return;
    setAsking(true);
    setAskError(null);
    setAnswer(null);
    try {
      const result = await askAssistant(question.trim());
      setAnswer(result.text);
    } catch (error) {
      setAskError(String(error));
    } finally {
      setAsking(false);
    }
  }
  return (
    <div className="page-stack">
      <div className="page-heading"><div>
        <h2>Local troubleshooting assistant</h2>
        <p>Read-only local findings, with optional questions to an organization-authorized model.</p>
      </div></div>
      {jobError ? <div className="notice notice-error">Job diagnostics unavailable. Connection and inventory checks are still shown.</div> : null}
      <section className="card">
        <CardHeader title="Diagnostic findings" description="Review evidence and choose a safe next step." />
        {findings.length ? findings.map((finding) => (
          <div className="definition-row" key={finding.title}>
            <div><strong>{finding.title}</strong><p>{finding.detail}</p></div>
            <button type="button" onClick={() => onNavigate(finding.next)}>Review</button>
          </div>
        )) : <p>No issue detected in the available status. This is not a complete runtime or policy attestation.</p>}
      </section>
      <section className="card">
        <CardHeader title="Ask managed model" description={assistant?.enabled ? assistant.model ?? "Managed model" : "Disabled or unavailable"} />
        {assistant?.enabled ? (
          <>
            <p>Submitting sends your question and coarse status counts to the organization's gateway. Do not include credentials, personal data or proprietary content. The model cannot execute actions.</p>
            <label htmlFor="assistant-question">Question</label>
            <textarea id="assistant-question" value={question} maxLength={2048}
              onChange={(event) => setQuestion(event.target.value)} disabled={asking} />
            <button type="button" disabled={asking || !question.trim()} onClick={() => void ask()}>
              {asking ? "Requesting…" : "Ask model"}
            </button>
            {askError ? <div className="notice notice-error" role="alert">{askError}</div> : null}
            {answer ? <div role="status" style={{ whiteSpace: "pre-wrap" }}>{answer}</div> : null}
          </>
        ) : <p>An administrator must provision a local assistant model and allow the dedicated gateway client.</p>}
      </section>
    </div>
  );
}
