# Multi-agent organisations

agentcore can run many agents together as an **organisation** made of
**departments** (a department is a "room"). Each department has its own
mission, data, tools and guardrails. Agents in the same department work
together. Departments talk to each other only through their
**communicator** agents. People can open any department and any agent, see
everything that happens, talk to any agent, and pause or stop one agent, a
department or the whole organisation.

An organisation can run on one VM or be spread over several VMs (**nodes**)
that share one PostgreSQL database.

- [Concepts](#concepts)
- [Building an organisation](#building-an-organisation)
- [Writing your own templates](#writing-your-own-templates)
- [Who may talk to whom](#who-may-talk-to-whom)
- [Messages](#messages)
- [Data, tools and guardrails per department](#data-tools-and-guardrails-per-department)
- [Working on a repository](#working-on-a-repository)
- [Goals, check-ins and agents that keep going](#goals-check-ins-and-agents-that-keep-going)
- [Human oversight](#human-oversight)
- [Limits](#limits)
- [Running on several VMs](#running-on-several-vms)
- [Data model](#data-model)
- [API](#api)
- [Configuration](#configuration)
- [What is not there yet](#what-is-not-there-yet)

## Concepts

```mermaid
flowchart TB
    subgraph ORG["Organisation (limits: max departments, max agents per department)"]
        subgraph R["Department: Research"]
            RC(["communicator"])
            R1["analyst"] <--> R2["writer"]
            R1 <--> RC
            R2 <--> RC
            RF[("department files")]
        end
        subgraph E["Department: Engineering"]
            EC(["communicator"])
            E1["developer"] <--> E2["reviewer"]
            E1 <--> EC
            E2 <--> EC
            EF[("department files")]
        end
        subgraph S["Department: Sales"]
            SC(["communicator"])
            S1["account manager"] <--> SC
        end
        RC <== "department to department" ==> EC
        EC <== "department to department" ==> SC
        RC -. "to all departments" .-> EC & SC
    end
    H["You (web UI)"] -. "watch · talk · pause · stop" .-> ORG
```

| Concept | What it is |
|---|---|
| **Organisation** | Every department on this agentcore installation, across all nodes. |
| **Department** (room) | A group of agents with a mission, its own data (department files, sandbox workspaces), its own tool set and its own guardrail policy. |
| **Worker agent** | A member of one department that does the department's work. It may talk to everyone in its department, including the communicator. |
| **Communicator agent** | Exactly one per department, created with the department. It has *only* messaging tools: no sandbox commands, no file access, no department files. It passes messages between its department and other departments. |
| **Node** | One agentcore process (usually one VM) that runs agent sandboxes. All nodes share PostgreSQL. |

Every agent is a normal agentcore session underneath: it runs in its own
sandbox, every action goes through policy and possibly a human approval, it
has a hash-chained audit log, a live view and a recording. The organisation
adds membership, messaging and coordinated control on top.

An agent's life:

```mermaid
stateDiagram-v2
    [*] --> stopped: added to a department
    stopped --> running: Start (placed on a node)
    running --> paused: Pause agent / department
    paused --> running: Resume
    running --> stopped: Stop · session ends · node lost
    paused --> stopped: Stop
    state running {
        [*] --> working
        working --> waiting: turn ends
        waiting --> working: a message arrives (wake-up)
    }
```

## Building an organisation

Nobody has to design an organisation from scratch. agentcore ships
**department templates** for the functions businesses usually need and
**growth paths** (blueprints) that add them a few at a time. Everything
created from a template is an ordinary department afterwards: rename it,
change its mission, add or remove agents.

```mermaid
flowchart LR
    A["1 · About your company<br/>name · what it does"] --> B{"2 · How to start"}
    B -- "Start small" --> S["first stage of a path,<br/>core agents only"]
    B -- "Grow in stages" --> G["a path, the first<br/>N stages"]
    B -- "Everything at once" --> F["every stage,<br/>full teams<br/>(limits raised if needed)"]
    B -- "Pick departments" --> P["any departments<br/>from the catalogue"]
    S & G & F & P --> R["3 · Review: plan, team size,<br/>which agent runs them, start now?"]
    R --> O["Organisation"]
    O --> N["Grow: next stage · related<br/>departments · more agents"]
    N --> R
```

### The catalogue

28 department templates in five groups (`templates/departments/*.toml`):

| Group | Departments |
|---|---|
| Product & engineering | Engineering, Quality Assurance, Platform & DevOps, Security, Product, Design, Data & Analytics, AI & Machine Learning, Technical Writing, Internal IT |
| Growth | Marketing, Content, Sales, Partnerships, Communications & PR, Community |
| Customers | Customer Support, Customer Success |
| Operations | Operations, Finance, Legal & Compliance, People & HR, Recruiting, Procurement, Supply Chain & Logistics |
| Leadership | Strategy & Leadership, Project Management, Market Research |

Each template has a mission (`{company}` is replaced with the company
name), the tools it needs (engineering-type departments get a sandbox;
business departments get department files), the departments it usually
works with, a "when to add it" hint, and 1–3 agents with instructions. Agents
marked `core` form a **lean** team; a **full** team has all of them.

### Growth paths

| Path | For | Stages |
|---|---|---|
| Solo software engineer | one person building software | Engineering → QA → DevOps, Docs |
| Content & marketing studio | creators, small marketing teams | Content, Marketing → Design, PR, Community → Data, Sales |
| Software product team | a product team | Engineering, Product → Design, QA → DevOps, Security → Data, Docs |
| SaaS startup | a software business | Engineering, Product → Marketing, Support → Sales, Success, Data → Finance, Legal, People |
| Online shop | physical products online | Operations, Marketing, Support → Content, Data, Supply chain → Finance, Procurement, Legal |
| Agency / services company | client projects | Projects, Engineering, Design → Sales, Marketing → Finance, Legal, People |
| Complete company | everything from day one | 5 stages, 27 departments |

### Starting small, growing big

* **Start small**: the first stage of a path with lean teams, e.g. a single
  Engineering department with a tech lead and a developer.
* **Grow in stages**: choose how many stages to create now.
* **Everything at once**: a whole path with full teams. If it does not fit
  the limits, the review says by how much and an admin can raise them in the
  same step.
* **Pick departments**: tick departments in the catalogue.

Afterwards the organisation page shows **suggestions**, most useful first:

1. the **next stage** of the chosen path;
2. **departments that work with the ones you have** (Engineering suggests
   Product, QA, DevOps, Design), with the reason to add them;
3. **more agents** for existing departments (a reviewer for Engineering).

Each one is a click away, and a suggestion that a limit blocks says so.
Admins build departments; operators can add the suggested agents; viewers
see the suggestions.

Teams add their own templates and paths by adding TOML files to
`templates/` (`[templates].dir`); they are validated at startup. See
[Writing your own templates](#writing-your-own-templates).

The company name and description (set in the builder, or with
`PUT /api/v1/org/profile`) are given to every agent at the top of its first
prompt, and `{company}` in template texts is replaced with the name.

## Writing your own templates

Department templates live in `templates/departments/*.toml`; each file holds
any number of `[[departments]]`:

```toml
[[departments]]
id = "engineering"                # unique; blueprints and pairs_with refer to it
name = "Engineering"              # name of the department when created
category = "build"                # build | grow | serve | run | lead
summary = "Designs, builds and maintains the software."
when_to_add = "First, if {company} builds software."   # shown in suggestions
tools = ["sandbox", "files"]      # sandbox and/or files; messaging is always on
policy = "department"             # optional; default: the `department` policy
pairs_with = ["product", "qa"]    # suggested once this department exists
mission = """Build and maintain {company}'s software. ..."""

[[departments.agents]]
name = "lead"                     # a-z 0-9 - _, how colleagues address it
title = "Tech lead"
core = true                       # part of a lean team (at least one per template)
instructions = """..."""
```

Blueprints (growth paths) are one file each in `templates/blueprints/`:

```toml
id = "solo-developer"
title = "Solo software engineer"
level = "starter"                 # starter | growing | complete
focus = "software"                # free text, shown to people
audience = "You build software on your own and want AI colleagues to help."
description = "..."

[[stages]]
title = "Your engineering department"
description = "A tech lead and a developer."
departments = ["engineering"]     # template ids; each at most once per blueprint
```

agentcore refuses to start if a template is invalid: duplicate ids, an
unknown category, tool group, policy or referenced template, an invalid or
duplicate agent name, or a template without a `core` agent. Templates are
only a starting point: departments created from them can be changed freely
and are not updated when the template changes.

## Who may talk to whom

Routing is decided by one pure function (`agentcore_core::org::route`), used
for every message, from agents and from people alike, and unit tested rule by
rule.

| From ↓ / To → | worker, same dept | own communicator | own department room | worker, other dept | communicator, other dept | other department | all departments |
|---|---|---|---|---|---|---|---|
| **Worker** | ✅ | ✅ | ✅ | ❌ | ❌ | ❌ | ❌ |
| **Communicator** | ✅ | – | ✅ | ❌ | ✅ | ✅ (to its communicator) | ✅ (to every other communicator) |
| **Human** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

A refused message is not stored or delivered. The agent gets an error that
explains the rule ("ask your department's communicator").

```mermaid
sequenceDiagram
    autonumber
    participant A as analyst (Research)
    participant RC as communicator (Research)
    participant EC as communicator (Engineering)
    participant D as developer (Engineering)

    A->>RC: team_send_message(to: "communicator", "We need the API rate limits")
    Note over RC: woken up with the message
    RC->>EC: team_send_to_department("Engineering", "Research asks: what are the API rate limits?")
    Note over EC: woken up with the message
    EC->>D: team_send_message(to: "everyone", ...)
    D->>EC: team_send_message(to: "communicator", "100 req/s per key")
    EC->>RC: team_send_to_department("Research", "100 req/s per key")
    RC->>A: team_send_message(to: "analyst", "Engineering says: 100 req/s per key")
    A--xD: team_send_message(to: "developer") ✗ refused: other department
```

## Messages

Messages are rows in PostgreSQL: the sender, the addressee (an agent, a
department room, or all departments), the scope (`internal`,
`inter_department`, `human`) and the text. When a message is sent, its
recipients are resolved once and an **inbox** entry is written per recipient:

| Addressed to | Recipients |
|---|---|
| an agent | that agent |
| own department room (from a member or a person) | everyone in the department except the sender |
| another department (communicator) | that department's communicator |
| all departments (communicator) | the communicators of all other departments |

Delivery to an agent:

```mermaid
flowchart LR
    S["team_send_message<br/>(tool call)"] --> P{"policy +<br/>approval"}
    P -- allowed --> RT{"route()"}
    RT -- refused --> ERR["error to the sender"]
    RT -- ok --> DB[("org_messages +<br/>org_inbox")]
    DB --> N["NOTIFY agentcore_org"]
    N --> NODE["node that runs<br/>the recipient"]
    NODE --> W{"recipient<br/>waiting for input?"}
    W -- yes --> WAKE["new turn with the message<br/>event: messages_delivered"]
    W -- "no (busy)" --> PULL["stays in the inbox:<br/>team_read_messages, or<br/>delivered when the turn ends"]
    W -- paused --> HOLD["held until resumed"]
```

* Sending is a tool call, so it goes through the department's policy like any
  other action: a policy can, for example, require a human to approve every
  message that leaves the department (`team_send_to_department`).
* Delivery is recorded in the recipient's audit log
  (`messages_delivered`, with message ids, senders and texts), so each agent's
  hash chain shows what it was told and by whom.
* An agent that can continue a conversation (opencode, CLI agents with
  `follow_up_args`) waits after each turn and is woken by new messages.
* Loops between agents are bounded by the policy limits of each session
  (`max_actions`, `max_model_calls`, `max_session_secs`).

### Agent tools

| Tool | Worker | Communicator | Purpose |
|---|---|---|---|
| `team_list_colleagues` | ✅ | ✅ | Members of the own department, their status, and the names of the other departments. |
| `team_send_message` | ✅ | ✅ | `to`: a colleague's name, `communicator` or `everyone` (the department room). |
| `team_read_messages` | ✅ | ✅ | Unread messages from the inbox (marks them delivered). |
| `team_send_to_department` | ❌ | ✅ | `department`: a department name or `all`. |
| `team_list_files`, `team_read_file`, `team_write_file` | if the department grants `files` | ❌ | The department's shared files. |
| `team_list_goals` | ✅ | ❌ | The organisation's goals with their latest progress. |
| `team_report_progress` | ✅ | ❌ | `goal` (id or title), `text`: replaces the goal's latest progress note. |
| `run_command`, `read_file`, `write_file`, `list_files` | if the department grants `sandbox` | ❌ | Commands and files in the agent's own sandbox. |

The tool list an agent sees *is* its permission: a tool that is not offered is
also refused if called.

## Working on a repository

A department can work on a **project**: a GitHub repository set up under
Projects (see [Ways of working](WAYS_OF_WORKING.md)). Link it when creating
or editing the department, or choose the repository in the builder: then
every new department whose template has a role on a repository is linked.

```mermaid
flowchart LR
    P["Project<br/>acme/shop"] --- E["Engineering<br/>role: developer"]
    P --- PR["Product<br/>role: project-manager"]
    P --- MK["Marketing<br/>role: marketing"]
    E -- "each worker: own checkout<br/>and branch" --> W["agent/task-engineering-developer-…"]
    W -- "propose_pull_request" --> D["Deliver (a person):<br/>checks · push · pull request"]
    PR -- "GitHub tools:<br/>issues, milestones, board" --> GH["GitHub"]
    MK -- "discussions, drafts" --> GH
```

What a worker in a linked department gets:

| | With the `sandbox` tool group | Without |
|---|---|---|
| Checkout | Its own clone of the repository (base branch from the project) on its own branch `agent/task-<department>-<agent>-<id>`, prepared on the host before the agent starts, exactly as for project sessions | none |
| GitHub tools | Those of the department's role (default: the project's role): developer → read issues, comment, propose a pull request; project-manager → manage issues, milestones and the board; marketing → discussions | same |
| First prompt | The role's playbook, the team's convention files from the repository (`CONTRIBUTING.md`, `AGENTS.md`, ...), the project's notes, the role's checks, then the department part (mission, colleagues, the repository and how to deliver) | the role's playbook and the project's notes, then the department part |
| Delivery | When the agent proposes a pull request, a person clicks **Deliver** in its session: the role's checks run, the branch is pushed with agentcore's GitHub credentials, the pull request is opened | – |

Communicators never get the repository. Nothing reaches GitHub without
passing the department's policy: GitHub writes need approval under the
`department` and `default` policies, and code only leaves the sandbox
through a delivery a person starts. Department templates for code-heavy
functions (Engineering, QA, DevOps, Security, Data, AI/ML, Technical
Writing) use the `default` policy, which allows the usual development tools
in the sandbox.

Each new session of an agent starts from a fresh checkout of the base
branch on a new branch, so work should be delivered before a session ends
(see the session limits in the policy).

## Goals, check-ins and agents that keep going

An organisation can work towards goals over days and weeks without a person
writing to it every time. Three things make that possible.

**Goals.** People set the organisation's goals ("Reach 10 paying customers
by March"), optionally owned by one department. Every agent sees the active
goals in its prompt; workers read them with `team_list_goals` and report
progress with `team_report_progress` (the latest note, who wrote it and when
are shown with the goal). Only people mark a goal *achieved* or *dropped*.

**Check-ins.** A check-in is a message sent on a schedule (every hour, day,
week, ...) to one agent or to everyone in a department, from
`check-in: <name>`. It wakes sleeping agents and tells them what to do, for
example the daily check-in: *"Read the goals, your notes and new messages.
Decide the most valuable next step towards the goals, do it, update your
notes, and report progress."* Every node runs the scheduler; each due
check-in is claimed by exactly one node (row lock with `SKIP LOCKED`), so it
is sent once. A check-in that was due several times while nothing ran (all
nodes down) is sent once and continues from now. People can run a check-in
now, turn it off, or delete it.

**Sleep, wake and continue.** An agent started by a person stays wanted
(`desired = running`) until a person stops it. When its session ends:

| The session ended because | The agent |
|---|---|
| nobody wrote to it within `idle_timeout_secs`, or a single-run agent finished | **sleeps** (`status = asleep`): no session, no sandbox, no cost. The next message (from a colleague, a person or a check-in) wakes it in a fresh session with the messages in its prompt. |
| it reached `max_session_secs` | **continues** at once in a fresh session (in a paused department: sleeps until resumed). |
| a person stopped it, it failed, ... | is stopped, as before. |

A fresh session starts from the agent's notes: workers with department files
keep `notes/<agent>.md` (what they are working on, decisions, next steps),
and the prompt of a continued or woken session says *"You are continuing"*
and asks the agent to read its notes and recent messages first. On a linked
repository it gets a fresh checkout on a new branch, so agents are told to
deliver work before a session ends. Sleeping agents survive a node restart
(they have nothing running); pausing or stopping works on them as on any
agent.

To stop a loop (an agent that ends at once and is woken again, two agents
waking each other), a node starts one agent automatically at most 6 times an
hour; after that the agent is stopped with a note. Starting it by hand
resets the count. Time budgets, model-call limits and the policy still apply
to every session.

The builder can set both up: *A goal for the organisation* creates the first
goal, and *Keep it running* adds a daily check-in for the lead of each new
department and a weekly review in Strategy.

## Data, tools and guardrails per department

| | Set per department | Enforced by |
|---|---|---|
| **Mission** | Text given to every agent of the department in its first prompt. | Prompt |
| **Tools** | `sandbox` (commands and files in the agent's own sandbox) and/or `files` (department files). Messaging is always available. | Tool gateway (`tools/list` and `tools/call`) |
| **Data** | Department files: a small shared file store (PostgreSQL, so every node sees it) that only that department's workers can read and write. Each worker also has its own sandbox workspace. | Gateway: the department is taken from the agent's identity, never from tool arguments |
| **Guardrails** | A policy (allow / deny / require approval, limits) for its workers. Communicators use the `communicator` policy. | Policy engine, per action |
| **Agent kind** | Which configured `[[agents]]` (e.g. `opencode`) each member runs, plus member-specific instructions. | Runtime |

The communicator has no sandbox tools, no file tools and a policy that only
allows the `team_*` messaging tools.

## Human oversight

Everything a person can do, at every level:

| | Agent | Department | Organisation |
|---|---|---|---|
| **See** | Live view, timeline, model calls, approvals, audit log (the existing session view) | Room: all members with status, every message in and out, department files | Overview: all departments, all agents, node health, organisation-wide message feed |
| **Talk** | Message an agent (wakes it) | Post in the department room (every member receives it) | Message any department |
| **Pause / resume** | ✅ | ✅ every agent in it, and new starts wait | ✅ (`Pause all`) |
| **Stop** | ✅ | ✅ every agent in it | ✅ **Stop all agents** (on every node) |

Pause and stop are recorded with who did it, in the audit log of every
affected agent and in the admin log.

## Limits

An admin sets, in the UI (Organisation → Limits) or with
`PUT /api/v1/org/settings`:

* `max_departments`: how many departments may exist;
* `max_agents_per_department`: how many worker agents a department may
  have (its communicator is not counted).

They are checked inside the same database transaction that creates the
department or agent (with a row lock on the settings), so two people adding
agents at the same time on different nodes cannot exceed them. Lowering a
limit does not remove anything; it only blocks new additions. In addition,
each node has a capacity (`[cluster].max_agents`): the number of agents it
runs at the same time.

## Running on several VMs

Every VM runs the same `agentcore serve` binary with its own
`[cluster].node_name`. All nodes use one PostgreSQL database. There is no
separate control-plane process: any node can serve the web UI and the API.

```mermaid
flowchart TB
    U["Browser"] --> LB["Load balancer / any node"]
    subgraph VM1["VM 1 · node vm-1"]
        N1["agentcore serve<br/>API · UI · gateways ·<br/>reconciler"] --> SB1["sandboxes<br/>(agents placed here)"]
        D1[("audit logs ·<br/>recordings ·<br/>workspaces")]
    end
    subgraph VM2["VM 2 · node vm-2"]
        N2["agentcore serve"] --> SB2["sandboxes"]
        D2[("audit logs · ...")]
    end
    LB --> N1 & N2
    N1 <-- "SQL + LISTEN/NOTIFY" --> PG[("PostgreSQL<br/>desired state · messages ·<br/>department files · node registry")]
    N2 <-- "SQL + LISTEN/NOTIFY" --> PG
    N1 <-. "proxied session API<br/>(internal_url)" .-> N2
    SB1 -- "MCP + LLM gateway<br/>(own node only)" --> N1
    SB2 -- "MCP + LLM gateway" --> N2
```

### Principles

1. **PostgreSQL holds the desired state; nodes converge to it.** Starting,
   pausing and stopping an agent or a department only *writes* what should be
   true (`org_agents.desired`, `departments.state`). Each node runs a
   **reconciler** that compares that with the sessions it runs and acts.
   A `NOTIFY agentcore_org` wakes every node's reconciler at once; a periodic
   pass (every 2 s) catches anything a dropped connection missed. So commands
   are level-triggered: a missed notification delays a command, it never loses
   it.
2. **A session is owned by exactly one node**, the one it was placed on. Its
   sandbox, audit log, recording and the agent's gateways live there. The
   agent only ever talks to its own node.
3. **Any node answers any request.** Session endpoints
   (`/api/v1/sessions/{id}/...`: details, events, live view, pause, stop,
   approvals, audit) are forwarded to the owning node's `internal_url`,
   including streams (SSE). The caller's token is forwarded and checked again
   by the owner, so every node needs the same `[[server.operators]]`.

### Placement

When an agent is started, the node handling the request picks a node inside
one transaction (with an advisory lock, so concurrent starts do not
overbook): among nodes whose heartbeat is fresh, the one with the lowest
load = running agents ÷ `max_agents`, skipping full nodes. No free node →
`503`. The agent row records the node; only that node's reconciler starts the
session.

### Reconciler (per node)

```mermaid
flowchart TD
    T["every 2 s, or NOTIFY"] --> L["agents placed on this node"]
    L --> A{"desired ≠ stopped and<br/>no session yet?"}
    A -- yes --> START["start the session<br/>(prompt with mission, colleagues, rules)"]
    L --> B{"session here,<br/>desired = stopped?"}
    B -- yes --> STOP["stop it"]
    L --> C{"desired = paused or<br/>department paused?"}
    C -- "yes, running" --> PAUSE["pause it"]
    C -- "no, paused" --> RESUME["resume it"]
    L --> D{"session waiting for input<br/>and unread inbox?"}
    D -- yes --> WAKE["deliver: new turn"]
    L --> E{"session gone<br/>(ended, node restarted)?"}
    E -- yes --> DONE["desired = stopped,<br/>last status recorded"]
```

### Failure handling

| What fails | What happens |
|---|---|
| A node crashes or loses the database | Its heartbeat goes stale (`node_timeout_secs`). Any other node marks that node's agents `stopped` (status `failed`, "node lost"). Their sandboxes died with the node; agents are not silently restarted elsewhere, because their workspace state is gone. A person starts them again. |
| A node restarts | Its interrupted sessions are closed in their audit logs (`session_ended`, interrupted) and its leftover sandboxes are removed. Only its *own* sessions: other nodes' live sessions are untouched. |
| A NOTIFY is lost | The periodic reconcile pass applies the change within 2 s. |
| The owning node is unreachable when a session page is opened | `503 node <name> is unreachable`; the organisation views (served from PostgreSQL) keep working. |
| Stop all agents | Writes `desired = stopped` for every agent and broadcasts `stop_all`; every node also stops sessions that do not belong to an organisation. |

### Security between nodes

* Nodes talk to each other only to forward session API calls, to
  `internal_url`, which should be on a private network. The forwarded request
  carries the operator's own token; the owner authenticates it again.
* Agents never reach another node: their gateway URL and token are those of
  their own node.
* PostgreSQL is the trust anchor: whoever can write to it can direct agents.
  Use TLS and a dedicated database user.
* `data/master.key` (or `$AGENTCORE_MASTER_KEY`) must be the same on every
  node, so each node can decrypt provider keys.

### Sizing

PostgreSQL `LISTEN/NOTIFY` and the 2 s reconcile pass are cheap at the scale
the limits allow (tens of departments × tens of agents). Payloads are only
nudges (`{"kind":"message","agents":[...]}`); the data is read with
indexed queries. The bus is one module (`cluster.rs`), so a dedicated broker
(NATS, Redis streams) can replace it if organisations grow far beyond that.

## Data model

```mermaid
erDiagram
    org_settings {
        int max_departments
        int max_agents_per_department
        text company_name
        text company_about
        text blueprint
    }
    departments ||--o{ org_agents : "members (1 communicator)"
    departments ||--o{ department_files : "shared data"
    org_agents ||--o| sessions : "current session"
    org_messages ||--o{ org_inbox : "one per recipient"
    org_agents ||--o{ org_inbox : "inbox"
    nodes ||--o{ org_agents : "placed on"
    nodes ||--o{ sessions : "owns"
    departments ||--o{ org_goals : "owns (optional)"
    departments ||--o{ org_schedules : "check-ins"
    org_agents ||--o{ org_schedules : "addressed to (optional)"

    departments {
        uuid id PK
        text name UK
        text mission
        text policy
        text_array tools
        text communicator_agent
        text state "active | paused"
        text template "created from"
        uuid project_id FK "works on"
        text role "role on the repository"
    }
    org_agents {
        uuid id PK
        uuid department_id FK
        text name
        text kind "worker | communicator"
        text agent "configured [[agents]]"
        text instructions
        text desired "stopped | running | paused"
        text node FK
        uuid session_id
        text status "... | asleep"
        text note
        text changed_by
    }
    org_goals {
        uuid id PK
        text title
        text description
        uuid department_id FK
        text status "active | achieved | dropped"
        text progress "latest note"
        text progress_by
        timestamptz progress_at
    }
    org_schedules {
        uuid id PK
        uuid department_id FK
        uuid agent_id FK "null: whole department"
        text name
        text message
        int every_minutes
        timestamptz next_run_at
        timestamptz last_run_at
        bool enabled
    }
    org_messages {
        uuid id PK
        text from_kind "human | agent"
        uuid from_agent
        uuid from_department
        text to_kind "agent | department | all_departments"
        uuid to_agent
        uuid to_department
        text scope
        text text
    }
    org_inbox {
        uuid message_id PK
        uuid agent_id PK
        timestamptz delivered_at
    }
    department_files {
        uuid department_id PK
        text path PK
        bytea content
    }
    nodes {
        text name PK
        text internal_url
        int capacity
        timestamptz last_seen
    }
```

## API

All under `/api/v1`; reading needs a viewer, changing an operator, settings
an admin.

| Method & path | What |
|---|---|
| `GET /org` | Overview: settings, departments with their agents, goals, check-ins, nodes. |
| `GET /org/stream` | SSE of change notifications (`agents`, `department`, `message`, `files`, `settings`, `goals`, `checkins`, `stop_all`, `resync`); clients refetch what changed. |
| `GET`/`POST /org/goals` | Goals; create `{"title", "description", "department_id"?}` (operators). |
| `PUT`/`DELETE /org/goals/{id}` | Change `title`, `description`, `department_id` (`null` clears it), `status` (`active`\|`achieved`\|`dropped`); delete. |
| `POST /org/goals/{id}/progress` | A person records progress: `{"text"}`. |
| `GET`/`POST /org/departments/{id}/checkins` | Check-ins of a department; create `{"name", "message", "every_minutes", "agent_id"?, "first_run_at"?}` (operators; default first run: one interval from now). |
| `PUT`/`DELETE /org/checkins/{id}` | Change `name`, `message`, `every_minutes`, `enabled`; delete. |
| `POST /org/checkins/{id}/run` | Send it now (`409` when turned off); the schedule continues from now. |
| `GET`/`PUT /org/settings` | Limits (admin). |
| `POST /org/departments` | Create a department (and its communicator): `{"name", "description", "mission", "policy", "tools", "communicator_agent", "project_id"?, "role"?}`. |
| `PUT`/`DELETE /org/departments/{id}` | Update; delete (only when all its agents are stopped). |
| `POST /org/departments/{id}/agents` | Add a worker agent. |
| `PUT`/`DELETE /org/agents/{id}` | Update instructions; remove (when stopped). |
| `POST /org/agents/{id}/start` · `pause` · `resume` · `stop` | Control one agent. |
| `POST /org/departments/{id}/start` · `pause` · `resume` · `stop` | Control every agent in a department. |
| `POST /org/pause-all` · `resume-all` | Pause or resume every department. |
| `POST /stop-all` | Stop everything, on every node. |
| `GET /org/messages?department=&agent=&after=&limit=` | Message feed (organisation, department or agent). |
| `POST /org/messages` | A person sends a message: `{"to": {"agent": id} \| {"department": id} \| "all_departments", "text": ...}`. |
| `GET /org/departments/{id}/files` · `GET`/`PUT /org/departments/{id}/files/{*path}` | Department files (`PUT` body `{"content": ...}`, operators). |
| `GET /org/templates` | Template categories, department templates and blueprints. |
| `GET`/`PUT /org/profile` | Company name and description, chosen blueprint (admin to change). |
| `GET /org/suggestions` | What to add next: the blueprint's next stage, related departments, more agents; `blocked` says when a limit prevents it. |
| `POST /org/build` | Create departments from templates (admin): `{"profile", "departments": [ids], "size": "lean"\|"full", "agent", "communicator_agent", "project_id", "goal", "check_ins", "start", "raise_limits", "dry_run"}`; `goal` creates a first goal, `check_ins` a daily check-in per new department (and a weekly review in Strategy); with `project_id`, new departments whose template has a role are linked to that project. Existing departments are skipped; a plan over the limits is refused with `409` (body has the plan) unless `raise_limits`. |

## Configuration

```toml
[cluster]
node_name = "vm-1"                       # unique per node (default: hostname)
internal_url = "http://10.0.0.11:8080"   # how other nodes reach this one
max_agents = 20                          # agents this node runs at once
heartbeat_secs = 5
node_timeout_secs = 30
reconcile_millis = 2000                  # periodic reconcile pass

[templates]
dir = "templates"                        # departments/*.toml, blueprints/*.toml

[org]
communicator_policy = "communicator"     # policy for every communicator
default_max_departments = 10             # initial limits (then set in the UI)
default_max_agents_per_department = 10
```

A single-VM installation needs no `[cluster]` section. Step-by-step setup of
several VMs: [Deployment](DEPLOYMENT.md#several-vms).

## What is not there yet

* Check-ins at a time of day (cron-like); today they run every N minutes
  from when they were created or last run.
* Read-only business data (analytics, revenue, support tickets) for the
  departments that steer the company.
* Moving a running agent between nodes (live migration of a sandbox).
* Per-department model provider and spend limits.
* A dedicated message broker for very large organisations.
