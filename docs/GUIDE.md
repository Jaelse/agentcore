# Guide: run your app with an organisation of agents

This guide walks through giving agentcore an app (a GitHub repository) and
an organisation of agents that maintains and grows it, step by step, in the
web UI. Every step links to the reference documentation.

What to expect: the organisation works on its own between your check-ins
(code as pull requests, drafts for customers, analyses, proposals), but
**people stay in charge**. You review pull requests, approve every message
that leaves the organisation, apply improvements and set the budget.
Whether the app becomes profitable depends on the product, the market and
the models you use; treat the organisation as a team you steer, not an
autopilot.

- [1. Install and sign in](#1-install-and-sign-in)
  - [Getting around](#getting-around)
- [2. Models, prices and a budget](#2-models-prices-and-a-budget)
- [3. Connect the repository](#3-connect-the-repository)
- [4. Build the organisation](#4-build-the-organisation)
- [5. Give it facts](#5-give-it-facts)
- [6. Let it talk to people](#6-let-it-talk-to-people)
- [7. Add the retrospective](#7-add-the-retrospective)
- [8. Your routine](#8-your-routine)
- [9. Growing](#9-growing)
- [10. Staying safe](#10-staying-safe)
- [Troubleshooting](#troubleshooting)

## 1. Install and sign in

Follow [Deployment](DEPLOYMENT.md): `docker compose up`, create an admin
token with `agentcore hash-token`, and sign in. Give other people the
`operator` role (they can direct agents and approve) or `viewer` (they can
only watch). Keep admins few: admins manage keys, budgets, data and
channels.

### Getting around

The **top bar** is on every page: the **agentcore** name goes back to Home,
then **Home**, **Organisation**, **Projects** and **Settings** (not for
viewers). On the right: **❚❚ Pause all** (**▶ Resume all** once everything is
paused), which pauses every department, and **■ Stop all agents**. On a
phone, Pause all shows only its icon, and a tab bar at the bottom has **Home**,
**Org** and **Settings** (Projects and signing out are under Settings). A red
dot on Home means a session waits for someone to approve or answer.

Every page has its own address, so you can bookmark it, share it, reload it
and use the browser's back button:

| Address | Page |
|---|---|
| `/` | **Home**: what happened lately, spending, and the organisation floor. |
| `/organisations` | The **organisation floor**: every department as a room, its members at their desks. |
| `/organisations/all` · `dashboard` · `improvements` · `outbox` · `releases` · `spending` · `data` | The sections in the Organisation sidebar (*Whole organisation*, 📊 Dashboard, 💡 Improvements, 📤 Outbox, 🚀 Releases, 💰 Spending, 🗄 Business data). |
| `/organisations/departments/<department>` | A **department**: its members, its room (every message), handoffs, check-ins, work items and files. |
| `/organisations/departments/<department>/members/<member>` | A **member**: its card (status, model, instructions, start, pause, stop, history) and a **conversation** with it. |
| `/organisations/departments/<department>/files` | The department's **files**: its shared files, each member's workspace, and the business data it may read. |
| `/sessions` · `/sessions/<session>` | Every session, and one session (live view, conversation, approvals, audit). Not in the menu: you get there from a member (*full session*, *History*), a project or Home. |
| `/projects` · `/settings` | Projects (repositories) and settings. |

On the floor (Home and Organisation), click a department's name to open it,
**📂** to open its files, or a member at its desk to open the member. A
member's desk shows how it is: working, *needs you* (**!**), asleep, paused or
stopped.

## 2. Models, prices and a budget

1. **Settings → Model providers → Add a provider**: the provider's API key
   stays on the server; agents only get a session token. Restrict
   `allowed_models` to the models you want to pay for.
2. **Settings → Agent catalogue**: add the open-source agent the
   departments should run (for example opencode or Codex CLI) with that
   provider and model. See [Agent catalogue](AGENT_CATALOG.md).
3. **Organisation → 💰 Spending → Prices**: enter the price per million
   input and output tokens for the models you use (from your provider's
   price list); set the currency.
4. **Spending → + Budget**: a budget for *the whole organisation*, per month,
   with *pause the work* at the limit. Start low (what you would pay a
   freelancer for a few days) and raise it once you trust the setup. Also
   set a spending limit at the provider: it is the hard stop.

Reference: [Spending and budgets](MULTI_AGENT.md#spending-and-budgets).

## 3. Connect the repository

1. **Settings → GitHub**: a token of a bot account (or a fine-grained
   token) with access only to the repositories agents work on.
2. **Projects → New project**: the repository, its default branch, and
   notes for the team (how to build and test, conventions). Protect the
   default branch with required reviews on GitHub, so every agent pull
   request needs a person.

Reference: [Ways of working](WAYS_OF_WORKING.md),
[Working on a repository](MULTI_AGENT.md#working-on-a-repository).

## 4. Build the organisation

**Organisation** opens the builder when there are no departments yet.

1. **How to start**: *Start small* (one department), *Grow in stages* (a
   growth path such as **SaaS startup** or **Solo software engineer**),
   *Everything at once*, or *Pick departments*. For an app, begin with
   *Grow in stages → SaaS startup*, first stage only: Engineering, Product,
   QA.
2. **Company**: name and one sentence about the app; every agent reads it.
3. **Review & create**:
   - *Lean* team (core agents only) to start;
   - *Works on a GitHub repository*: the project from step 3, so
     engineering-type departments get a checkout and deliver pull requests,
     and product-type departments get issues and the board;
   - *A goal for the organisation*: something measurable, for example
     "10 paying customers by March";
   - *Keep it running*: a daily check-in per department, so work continues
     without you writing to it;
   - *Start the new departments right away*.

Agents that have nothing to do **sleep** (no sandbox, no cost) and wake on
messages and check-ins; sessions that reach their time limit continue from
the agents' notes. Reference:
[Building an organisation](MULTI_AGENT.md#building-an-organisation),
[Goals, check-ins and agents that keep going](MULTI_AGENT.md#goals-check-ins-and-agents-that-keep-going).

## 5. Give it facts

Decisions are only as good as the facts behind them. **Organisation → 🗄
Business data → + Add data**:

- a **table** (CSV export of customers, revenue per month, support
  tickets);
- a **PostgreSQL** database: create a database user that can only `SELECT`
  the tables needed, and use its connection string;
- an **HTTP API** (analytics, billing): a read-only API key, and allowed
  paths.

Tick the departments that may read each source (Product and Data for usage,
Sales and Finance for revenue) and use *Try it* first. Reference:
[Business data](MULTI_AGENT.md#business-data).

## 6. Let it talk to people

When Sales, Support or Marketing should reach real people: **Organisation →
📤 Outbox → + Channel**.

- **Email** (SMTP) from a dedicated address, limited to the domains you
  expect where possible;
- **Slack** for internal updates (this one can *send without approval*);
- a **webhook** to Zapier, Make or n8n for social media or your CRM.

Keep *a person approves every message* on for anything customers or the
public see, and a low daily limit. Every message carries an AI disclosure.
Reference: [Talking to the outside world](MULTI_AGENT.md#talking-to-the-outside-world).

## 7. Add the retrospective

**Dashboard → Add a Retrospective agent** (or the builder: *Retrospective*).
It reads the metrics every day and proposes fixes: clearer instructions for
an agent that keeps failing, a check-in for a department nobody asks
anything, a lower budget for one that spends without results. You apply,
change, send back or reject each proposal under **💡 Improvements**.
Reference: [Metrics, the retrospective and improvements](MULTI_AGENT.md#metrics-the-retrospective-and-improvements).

## 8. Your routine

**Daily, about 15 minutes:**

| Where | What |
|---|---|
| 📤 Outbox | Approve, edit, send back or reject drafts. Customers wait while drafts wait. |
| GitHub | Review the agents' pull requests; comment like you would for a colleague. |
| Members marked **!** (*needs you*) on the floor | Open the member, then *full session*: approve or reject actions the guardrails held back. |
| 📊 Dashboard → What looks inefficient | Read the signals; *Ask for a fix* sends one to the retrospective. |
| 💡 Improvements | Decide on proposals. |

**Weekly:**

- **Goals** (Organisation overview): read the progress reports, mark goals
  achieved or dropped, add the next ones.
- **💰 Spending**: compare spend per department with what it delivered;
  adjust budgets.
- **Department rooms**: skim the conversation; correct misunderstandings by
  writing to the room. **📂 Files** on a department shows what it wrote:
  its shared files and each member's workspace.

Talk to the organisation any time: write to a department room, or tell
everyone (or one department) something that stands with **📣 Announce** on
the organisation floor; ask agents to acknowledge it when it matters.

**Ask a member about its work** on its page (click it on the floor, or
**💬 Talk** on its card): write, and its answers appear below your messages.
What happens depends on how it is:

| The member is | Your message |
|---|---|
| stopped | Starts it **only to answer you**: it is told a person wants to talk, answers from its notes and files, and does not take up its work. It is stopped again once the conversation goes quiet (the policy's `idle_timeout_secs`, an hour by default) or when you press **End conversation**. Its card says *💬 talking* meanwhile. It also works in a department that waits for a repository. |
| asleep | Wakes it. |
| waiting | Reaches it right away. |
| working | Reaches it when it finishes what it is doing. |
| paused | Waits until it is resumed. |

Pause a department (or everything, with **Pause all** in the top bar) when
you are away and do not want work to continue.

## 9. Growing

- On the organisation floor, each room's **Grow** row suggests the
  departments that work with it and the agents it is missing, and
  **+ Add department** adds any department from the catalogue or one of your
  own, with agents in the roles you choose. Raise the limits when asked.
- Add agents to busy departments; keep departments small and focused.
- More agents than one VM can run: add nodes ([Several VMs](DEPLOYMENT.md#several-vms)).

## 10. Staying safe

- Docker backend with gVisor for agents, never the `process` backend in
  production; see [Security](../SECURITY.md).
- Narrow access everywhere: GitHub token per repository, read-only database
  users, read-only API keys, channels only for the departments that need
  them.
- Approval stays on for anything outside the organisation; "apply without
  asking" stays empty until you trust the retrospective for a kind of
  change.
- An organisation budget with *pause*, plus the provider's own limit.
- Back up PostgreSQL, the audit logs and the master key (separately).

## Troubleshooting

| You see | Why | Do |
|---|---|---|
| Agent **💤 asleep** | It had nothing to do; its session ended to save money. | Nothing: a message or check-in wakes it. Add a check-in if it should work regularly. |
| Agent stopped: *started 6 times within an hour* | It ended right after starting again and again (a broken agent, or two agents waking each other). | Open its last sessions to see why; fix the agent or its instructions, then start it. |
| Department paused, changed by *budget* | A `pause` budget ran out. | Raise the limit under Spending, or wait for the next period (it resumes by itself). |
| Model calls refused: *budget ... is used up* | The department was resumed by hand while its budget is still used up. | Raise the budget or wait for the next period. |
| Spend shows "–" | No prices are set for the models used. | Add prices; then *Price them with the current prices*. |
| Outbox message **failed** | The channel refused it or was unreachable. | *Send a test* on the channel, fix the settings, *Retry*. |
| Outbox: *reached its limit of N messages today* | The channel's daily limit. | Wait until tomorrow (UTC), or raise the limit. |
| `data_query` errors in a session | Wrong SQL, a path outside the allowed ones, or the source was turned off. | *Try it* on the source with the same query. |
| Nothing happens for days | No check-ins, no goals, nobody writing to the departments. | Add goals and *daily* check-ins; the dashboard flags idle departments. |
| Agent *waiting for room* | The machine it ran on stopped and every other node runs as many agents as it can. | Add a node or raise `[cluster].max_agents`; it carries on by itself once there is room. |
| Session *interrupted: its node stopped, and it could not be carried on* | It was started before sessions could move between machines, or its agent is no longer configured. | Start it again. |
| Agent waits for a long time | An action waits for approval, or its department is paused. | Open the member → *full session* → approvals; resume the department. |
| Agent shows **💬 talking** | A person started it only to talk with it. | Nothing: it stops by itself once the conversation goes quiet. **End conversation** on its page stops it now; **Start** it afterwards to have it work. |
| *Member not found* or *Department not found* | The link points to something that was removed, or is mistyped. | Go back to the organisation floor. |
