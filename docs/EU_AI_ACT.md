# EU AI Act support

> This document explains how agentcore's features support the obligations of
> Regulation (EU) 2024/1689 (the "AI Act"). It is engineering documentation,
> **not legal advice**. Whether, and in which role (provider or deployer), the
> obligations apply to you depends on your use case. Involve your legal and
> compliance teams.

## Is an AI coding agent "high-risk"?

Usually not. General software-development assistance is not listed in
Annex III. It can become high-risk, or fall under other rules, depending on
what the agent is used for (for example, if its work feeds decisions about
employees, or it becomes a safety component of a regulated product).
agentcore is built so the high-risk requirements can be met anyway: they are
also good engineering practice for autonomous agents, and the transparency
(Art. 50) and AI literacy (Art. 4) duties apply regardless of risk class.

Application dates: Art. 4 and the prohibitions since 2 Feb 2025; GPAI model
obligations since 2 Aug 2025; most remaining obligations, including Art. 50 and
Annex III high-risk systems, from 2 Aug 2026; Annex I products from 2 Aug 2027.
Check for amendments (such as the "Digital Omnibus" proposals) that may shift
the high-risk dates.

## Mapping

| Article | Requirement (summary) | agentcore feature | Your responsibility |
|---|---|---|---|
| **Art. 9** Risk management | Identify, evaluate and mitigate risks throughout the lifecycle. | Policies encode risk decisions per action type; sandbox limits blast radius; limits on time and number of actions. | Run and document a risk assessment for your use cases; review policies regularly. |
| **Art. 11 / Annex IV** Technical documentation | Describe the system, its components and its logic. | `docs/ARCHITECTURE.md`, `docs/POLICIES.md`, the system card (`/api/v1/system-card`), policy digests. | Add your deployment details, models used and intended purpose. |
| **Art. 12** Record-keeping | Automatic logging of events over the system's lifetime, enabling traceability and post-market monitoring. | Every session event (task, initiator, policy + SHA-256 digest, sandbox details, each action, verdict, approval, outcome, output, stop, end) goes to a **hash-chained, append-only** log before anything else happens. Logging is fail-closed. Logs can be verified (`agentcore audit verify`, UI "Verify integrity") and exported (JSONL). | Store `data/audit` on durable storage; back it up; ship chain heads to WORM storage if you need truncation evidence. |
| **Art. 13** Transparency to deployers | Instructions for use: purpose, capabilities, limitations, human oversight measures, logging. | `[transparency]` config, published in the UI ("About this system") and at `/api/v1/system-card`; this documentation. | Fill in provider, contact, intended purpose and limitations accurately. |
| **Art. 14** Human oversight | Humans can understand and monitor the system, avoid automation bias, decide not to use, override, and *"interrupt the system through a 'stop' button or a similar procedure"* (Art. 14(4)(e)). | Live timeline and output; **STOP AGENT** and **Stop all agents** buttons that kill the sandbox immediately; approval gates with comments; rejection by default on timeout; `supervised` policy for full step-by-step control; the agent is told when it was denied. All interventions are attributed to an authenticated person. | Assign oversight to trained people with the authority to stop agents (Art. 26(2)); define when approvals are required. |
| **Art. 15** Accuracy, robustness, cybersecurity | Resilience against errors, faults and attempts to alter use or behaviour (e.g. prompt injection). | Defence in depth: hardened container sandbox (no capabilities, read-only rootfs, non-root, resource limits, no network), optional gVisor; policy engine with path normalisation and deny-overrides; per-session gateway tokens; secrets never in audited arguments; fail-closed audit. | Keep images patched; enable gVisor for untrusted code; restrict the network; protect the host (see SECURITY.md). |
| **Art. 19 / Art. 26(6)** Log retention | Keep automatically generated logs for at least six months (unless other law requires otherwise). | `storage.audit_retention_days` (default 183) is published in the system card. agentcore never deletes audit logs. | Implement retention and deletion in your storage, consistent with GDPR. |
| **Art. 26** Deployer obligations | Use according to instructions, monitor operation, inform affected workers, keep logs. | Viewer role for monitoring; session history; roles separate who can act from who can watch. | Inform workers' representatives before workplace deployment (Art. 26(7)); report serious incidents. |
| **Art. 50(1)** Interaction disclosure | People must know they are interacting with an AI system. | Persistent "AI system" badge and footer notice in the UI. | Disclose in other channels where agent output reaches people (PR descriptions, comments). |
| **Art. 50(2)** Marking AI-generated content | Synthetic content should be marked in a machine-readable way. | `AGENTCORE_AI_GENERATED=agentcore/ai-generated` is set for the agent and every command, so tooling (e.g. a git commit hook adding an `AI-Generated:` trailer) can mark output; write outcomes record content hashes, so audited output is attributable. | Configure your tooling to apply the marker (e.g. commit trailers, PR labels). |
| **Art. 4** AI literacy | Staff dealing with AI systems need sufficient AI literacy. | Clear UI wording; documentation of limitations. | Train operators and approvers. |

## What an audit record looks like

```json
{"seq":12,"recorded_at":"2026-10-04T17:10:48.130Z","prev_hash":"9c1f…","event":{"id":"…","session_id":"…","seq":12,"timestamp":"…","event":"approval_resolved","approval_id":"…","action_id":"…","approved":false,"by":{"kind":"human","id":"alice"},"comment":"not today"},"hash":"4be0…"}
```

`hash = SHA-256(seq ‖ recorded_at ‖ prev_hash ‖ event)` with length-prefixed
fields. Changing, inserting, deleting or reordering a record breaks
verification at that line.

## Gaps and roadmap

* LLM prompts and responses are not yet captured, because agents call the model
  provider directly. The planned model gateway will log them (Art. 12) and keep
  API keys out of the sandbox.
* The session index is in memory; audit files persist across restarts, but the
  UI only lists sessions from the current process.
* Operator identity is token based; OIDC/SSO is planned to tie interventions to
  corporate identities.
