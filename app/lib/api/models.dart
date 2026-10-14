// Mirrors the JSON of agentcore's operator API (see web/src/types.ts and
// docs/API.md). Only the fields the app shows are typed; the rest stay in
// `raw` where a screen needs them.

typedef Json = Map<String, dynamic>;

DateTime? _date(Object? v) => v is String ? DateTime.tryParse(v)?.toLocal() : null;
String _str(Object? v, [String fallback = '']) => v is String ? v : fallback;
int _int(Object? v) => v is num ? v.toInt() : 0;
List<String> _strings(Object? v) => v is List ? v.whereType<String>().toList() : const [];
List<Json> _objects(Object? v) => v is List ? v.whereType<Json>().toList() : const [];

enum Role {
  viewer,
  operator,
  admin;

  static Role parse(Object? v) => values.firstWhere((r) => r.name == v, orElse: () => viewer);

  bool get canOperate => index >= operator.index;
  bool get isAdmin => this == admin;
}

class Me {
  Me({required this.name, required this.role});
  factory Me.fromJson(Json j) => Me(name: _str(j['name']), role: Role.parse(j['role']));
  final String name;
  final Role role;
}

/// `{kind: human|agent, id}` or `{kind: system}`.
String principalLabel(Object? p) {
  if (p is! Json) return 'unknown';
  final kind = _str(p['kind']);
  return kind == 'system' ? 'system' : '$kind:${_str(p['id'])}';
}

enum SessionStatus {
  pending,
  running,
  awaitingApproval,
  awaitingInput,
  paused,
  stopped,
  completed,
  failed,
  unknown;

  static SessionStatus parse(Object? v) => switch (v) {
    'pending' => pending,
    'running' => running,
    'awaiting_approval' => awaitingApproval,
    'awaiting_input' => awaitingInput,
    'paused' => paused,
    'stopped' => stopped,
    'completed' => completed,
    'failed' => failed,
    _ => unknown,
  };

  bool get isTerminal => this == stopped || this == completed || this == failed;

  String get label => switch (this) {
    awaitingApproval => 'awaiting approval',
    awaitingInput => 'awaiting input',
    _ => name,
  };
}

class SessionInfo {
  SessionInfo({
    required this.id,
    required this.agent,
    required this.task,
    required this.policy,
    required this.status,
    required this.createdBy,
    required this.createdAt,
    required this.endedAt,
    required this.pendingApprovals,
    required this.actions,
    required this.modelCalls,
    required this.context,
  });

  factory SessionInfo.fromJson(Json j) => SessionInfo(
    id: _str(j['id']),
    agent: _str(j['agent']),
    task: _str(j['task']),
    policy: _str(j['policy']),
    status: SessionStatus.parse(j['status']),
    createdBy: principalLabel(j['created_by']),
    createdAt: _date(j['created_at']),
    endedAt: _date(j['ended_at']),
    pendingApprovals: _int(j['pending_approvals']),
    actions: _int(j['actions']),
    modelCalls: _int(j['model_calls']),
    context: (j['context'] as Json?) ?? const {},
  );

  final String id;
  final String agent;
  final String task;
  final String policy;
  final SessionStatus status;
  final String createdBy;
  final DateTime? createdAt;
  final DateTime? endedAt;
  final int pendingApprovals;
  final int actions;
  final int modelCalls;
  final Json context;

  String? get projectName => context['project_name'] as String?;
  String? get departmentName => context['department_name'] as String?;
  String? get pullRequestUrl => context['pull_request_url'] as String?;
  Json? get issue => context['issue'] as Json?;

  /// One line naming where the session belongs, if anywhere.
  String? get origin {
    final issue = this.issue;
    if (projectName != null) {
      return issue == null ? projectName : '$projectName #${issue['number']}';
    }
    return departmentName;
  }
}

/// Human-readable summary of a sandbox action (see `actionSummary` in the web UI).
String actionSummary(Object? action) {
  if (action is! Json) return 'action';
  return switch (action['type']) {
    'exec' => [_str(action['command']), ..._strings(action['args'])].join(' '),
    'file_read' => 'read ${_str(action['path'])}',
    'file_write' => 'write ${_str(action['path'])} (${_int(action['bytes'])} bytes)',
    'network' => 'connect ${_str(action['host'])}${action['port'] != null ? ':${action['port']}' : ''}',
    'tool_call' => 'tool ${_str(action['tool'])}',
    final other => '$other',
  };
}

class PendingApproval {
  PendingApproval({
    required this.approvalId,
    required this.actionId,
    required this.action,
    required this.reason,
    required this.requestedAt,
    required this.expiresAt,
  });

  factory PendingApproval.fromJson(Json j) => PendingApproval(
    approvalId: _str(j['approval_id']),
    actionId: _str(j['action_id']),
    action: (j['action'] as Json?) ?? const {},
    reason: _str(j['reason']),
    requestedAt: _date(j['requested_at']),
    expiresAt: _date(j['expires_at']),
  );

  final String approvalId;
  final String actionId;
  final Json action;
  final String reason;
  final DateTime? requestedAt;
  final DateTime? expiresAt;

  String get summary => actionSummary(action);
}

class SessionDetail {
  SessionDetail({
    required this.session,
    required this.live,
    required this.policyName,
    required this.approvals,
    required this.proposal,
    required this.roleTitle,
    required this.delivers,
  });

  factory SessionDetail.fromJson(Json j) {
    final role = j['role'] as Json?;
    return SessionDetail(
      session: SessionInfo.fromJson(j['session'] as Json),
      live: j['live'] == true,
      policyName: _str((j['policy'] as Json?)?['name']),
      approvals: _objects(j['approvals']).map(PendingApproval.fromJson).toList(),
      proposal: j['proposal'] as Json?,
      roleTitle: role?['title'] as String?,
      delivers: role?['delivery'] == 'pull_request',
    );
  }

  final SessionInfo session;
  final bool live;
  final String policyName;
  final List<PendingApproval> approvals;
  final Json? proposal;
  final String? roleTitle;

  /// Whether the session's role delivers a pull request.
  final bool delivers;
}

/// One entry of a session's tamper-evident event log.
class AgentEvent {
  AgentEvent(this.raw) : seq = _int(raw['seq']), kind = _str(raw['event']), timestamp = _date(raw['timestamp']);

  final Json raw;
  final int seq;
  final String kind;
  final DateTime? timestamp;

  /// Events that are noise in a phone-sized timeline.
  bool get isDetail => kind == 'policy_evaluated' || kind == 'action_requested';

  String get summary {
    final r = raw;
    return switch (kind) {
      'session_created' => 'Session created by ${principalLabel(r['created_by'])} (policy ${r['policy']})',
      'sandbox_started' => 'Sandbox started (${r['backend']})',
      'agent_started' => 'Agent started: ${r['program']}',
      'workspace_prepared' => 'Checked out ${r['repository']} on ${r['branch']}',
      'role_applied' => 'Role ${r['role']} applied',
      'turn_started' => 'Turn ${r['turn']} started',
      'turn_ended' => 'Turn ${r['turn']} ended${r['exit_code'] != null ? ' (exit ${r['exit_code']})' : ''}',
      'user_message' => '${principalLabel(r['by'])}: ${r['text']}',
      'checks_completed' => 'Checks ${r['passed'] == true ? 'passed' : 'failed'}',
      'pull_request_proposed' => 'Proposed PR: ${(r['proposal'] as Json?)?['title'] ?? ''}',
      'delivered' => 'Delivered ${r['branch']}${r['pull_request_url'] != null ? ' → ${r['pull_request_url']}' : ''}',
      'output' => _str(r['line']),
      'action_requested' => 'Requested: ${actionSummary(r['action'])}',
      'policy_evaluated' => 'Policy: ${(r['verdict'] as Json?)?['verdict']}',
      'approval_requested' => 'Needs approval: ${actionSummary(r['action'])} — ${r['reason']}',
      'approval_resolved' =>
        '${r['approved'] == true ? 'Approved' : 'Rejected'} by ${principalLabel(r['by'])}'
            '${r['comment'] != null ? ': ${r['comment']}' : ''}',
      'action_completed' => _outcome(r['outcome']),
      'model_call' =>
        'Model call ${r['provider']}/${r['model'] ?? '?'}: ${r['outcome']}'
            '${r['output_tokens'] != null ? ' (${r['input_tokens']}→${r['output_tokens']} tokens)' : ''}',
      'status_changed' => 'Status: ${SessionStatus.parse(r['status']).label}',
      'stop_requested' => 'Stop requested by ${principalLabel(r['by'])}: ${r['reason']}',
      'paused' => 'Paused by ${principalLabel(r['by'])}',
      'resumed' => 'Resumed by ${principalLabel(r['by'])}',
      'recording_closed' => 'Terminal recording saved',
      'session_ended' =>
        'Session ended: ${SessionStatus.parse(r['status']).label}'
            '${r['reason'] != null ? ' — ${r['reason']}' : ''}',
      _ => kind.replaceAll('_', ' '),
    };
  }

  static String _outcome(Object? o) {
    if (o is! Json) return 'Action completed';
    return switch (o['status']) {
      'succeeded' => 'Action succeeded',
      'failed' => 'Action failed: ${o['error']}',
      'denied' => 'Action denied: ${o['reason']}',
      _ => 'Action completed',
    };
  }
}

class AgentEntry {
  AgentEntry({required this.name, required this.description, required this.enabled});
  factory AgentEntry.fromJson(Json j) {
    final spec = (j['spec'] as Json?) ?? const {};
    return AgentEntry(name: _str(spec['name']), description: _str(spec['description']), enabled: j['enabled'] != false);
  }
  final String name;
  final String description;
  final bool enabled;
}

class Changes {
  Changes({
    required this.base,
    required this.commits,
    required this.files,
    required this.patch,
    required this.truncated,
  });
  factory Changes.fromJson(Json j) => Changes(
    base: _str(j['base']),
    commits: _objects(j['commits']),
    files: _objects(j['files']),
    patch: _str(j['patch']),
    truncated: j['patch_truncated'] == true,
  );
  final String base;
  final List<Json> commits;
  final List<Json> files;
  final String patch;
  final bool truncated;
}

// ---- organisation ----------------------------------------------------------

class OrgAgent {
  OrgAgent({
    required this.id,
    required this.name,
    required this.kind,
    required this.agent,
    required this.desired,
    required this.status,
    required this.sessionId,
    required this.note,
  });
  factory OrgAgent.fromJson(Json j) => OrgAgent(
    id: _str(j['id']),
    name: _str(j['name']),
    kind: _str(j['kind']),
    agent: _str(j['agent']),
    desired: _str(j['desired']),
    status: j['status'] as String?,
    sessionId: j['session_id'] as String?,
    note: j['note'] as String?,
  );
  final String id;
  final String name;

  /// `worker` or `communicator`.
  final String kind;
  final String agent;

  /// What people asked for: `stopped`, `running` or `paused`.
  final String desired;

  /// The status of its current session, if any.
  final String? status;
  final String? sessionId;
  final String? note;
}

class Department {
  Department({
    required this.id,
    required this.name,
    required this.description,
    required this.mission,
    required this.state,
    required this.agents,
  });
  factory Department.fromJson(Json j) => Department(
    id: _str(j['id']),
    name: _str(j['name']),
    description: _str(j['description']),
    mission: _str(j['mission']),
    state: _str(j['state']),
    agents: _objects(j['agents']).map(OrgAgent.fromJson).toList(),
  );
  final String id;
  final String name;
  final String description;
  final String mission;

  /// `active` or `paused`.
  final String state;
  final List<OrgAgent> agents;
}

class OrgOverview {
  OrgOverview({
    required this.companyName,
    required this.departments,
    required this.outboxPending,
    required this.releasesPending,
    required this.proposalsPending,
  });
  factory OrgOverview.fromJson(Json j) => OrgOverview(
    companyName: _str((j['profile'] as Json?)?['company_name']),
    departments: _objects(j['departments']).map(Department.fromJson).toList(),
    outboxPending: _int(j['outbox_pending']),
    releasesPending: _int(j['releases_pending']),
    proposalsPending: _int(j['proposals_pending']),
  );
  final String companyName;
  final List<Department> departments;
  final int outboxPending;
  final int releasesPending;
  final int proposalsPending;
}

class OrgMessage {
  OrgMessage({
    required this.id,
    required this.createdAt,
    required this.fromName,
    required this.toName,
    required this.text,
    required this.scope,
  });
  factory OrgMessage.fromJson(Json j) => OrgMessage(
    id: _str(j['id']),
    createdAt: _date(j['created_at']),
    fromName: _str(j['from_name']),
    toName: _str(j['to_name']),
    text: _str(j['text']),
    scope: _str(j['scope']),
  );
  final String id;
  final DateTime? createdAt;
  final String fromName;
  final String toName;
  final String text;

  /// `internal`, `inter_department` or `human`.
  final String scope;
}

// ---- outbox and releases ---------------------------------------------------

class OutboxItem {
  OutboxItem({
    required this.id,
    required this.draftedBy,
    required this.recipients,
    required this.subject,
    required this.body,
    required this.status,
    required this.revision,
    required this.createdAt,
  });
  factory OutboxItem.fromJson(Json j) => OutboxItem(
    id: _str(j['id']),
    draftedBy: _str(j['drafted_by']),
    recipients: _strings(j['recipients']),
    subject: _str(j['subject']),
    body: _str(j['body']),
    status: _str(j['status']),
    revision: _int(j['revision']),
    createdAt: _date(j['created_at']),
  );
  final String id;
  final String draftedBy;
  final List<String> recipients;
  final String subject;
  final String body;
  final String status;
  final int revision;
  final DateTime? createdAt;
}

class Release {
  Release({
    required this.id,
    required this.version,
    required this.notes,
    required this.status,
    required this.revision,
    required this.riskLevel,
    required this.riskAssessment,
    required this.approvalsRequired,
    required this.approvedBy,
    required this.proposedBy,
    required this.productId,
  });
  factory Release.fromJson(Json j) {
    final risk = (j['risk'] as Json?) ?? const {};
    return Release(
      id: _str(j['id']),
      version: _str(j['version']),
      notes: _str(j['notes']),
      status: _str(j['status']),
      revision: _int(j['revision']),
      riskLevel: _str(risk['level'], 'unknown'),
      riskAssessment: _str(risk['assessment']),
      approvalsRequired: _int(j['approvals_required']),
      approvedBy: _objects(j['approvals']).map((a) => _str(a['by'])).toList(),
      proposedBy: _str(j['proposed_by']),
      productId: _str(j['product_id']),
    );
  }
  final String id;
  final String version;
  final String notes;
  final String status;
  final int revision;
  final String riskLevel;
  final String riskAssessment;
  final int approvalsRequired;
  final List<String> approvedBy;
  final String proposedBy;
  final String productId;
}
