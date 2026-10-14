import 'dart:async';

import 'package:flutter/material.dart';

import '../api/client.dart';
import '../api/models.dart';
import '../state/app_state.dart';
import 'common.dart';

/// One session: what it is doing (live), what waits for a person, and the
/// controls to answer, pause or stop it.
class SessionScreen extends StatefulWidget {
  const SessionScreen({super.key, required this.sessionId});

  final String sessionId;

  @override
  State<SessionScreen> createState() => _SessionScreenState();
}

class _SessionScreenState extends State<SessionScreen> {
  late final ApiClient _client = AppScope.read(context).client!;
  SessionDetail? _detail;
  Object? _error;
  final _events = <AgentEvent>[];
  StreamSubscription<AgentEvent>? _sub;
  Timer? _refreshSoon;
  bool _showOutput = false;
  bool _showDetails = false;
  final _scroll = ScrollController();
  final _message = TextEditingController();

  static const _maxEvents = 3000;

  /// Events after which the header and approvals are refetched.
  static const _stateEvents = {
    'approval_requested',
    'approval_resolved',
    'status_changed',
    'session_ended',
    'paused',
    'resumed',
    'pull_request_proposed',
    'delivered',
  };

  @override
  void initState() {
    super.initState();
    _load();
    _sub = _client.sessionEvents(widget.sessionId).listen(_onEvent);
  }

  @override
  void dispose() {
    _sub?.cancel();
    _refreshSoon?.cancel();
    _scroll.dispose();
    _message.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    try {
      final detail = await _client.session(widget.sessionId);
      if (mounted) {
        setState(() {
          _detail = detail;
          _error = null;
        });
      }
    } on ApiException catch (e) {
      if (!mounted) return;
      if (e.unauthorized) {
        await AppScope.read(context).signOut(reason: 'Your token is no longer accepted. Sign in again.');
        return;
      }
      setState(() => _error = e);
    }
  }

  void _onEvent(AgentEvent e) {
    final atBottom = !_scroll.hasClients || _scroll.position.extentAfter < 80;
    setState(() {
      _events.add(e);
      if (_events.length > _maxEvents) _events.removeRange(0, _events.length - _maxEvents);
    });
    if (_stateEvents.contains(e.kind)) {
      // Events arrive in bursts; refetch once they settle.
      _refreshSoon?.cancel();
      _refreshSoon = Timer(const Duration(milliseconds: 300), _load);
    }
    if (atBottom) {
      WidgetsBinding.instance.addPostFrameCallback((_) {
        if (_scroll.hasClients) _scroll.jumpTo(_scroll.position.maxScrollExtent);
      });
    }
  }

  Future<void> _act(Future<void> Function() action, {String? done}) async {
    if (await runAction(context, action, done: done)) await _load();
  }

  Future<void> _decide(PendingApproval a, bool approved) async {
    String? comment;
    if (!approved) {
      comment = await promptText(
        context,
        title: 'Reject this action?',
        action: 'Reject',
        hint: 'Why (the agent sees this)',
        destructive: true,
      );
      if (comment == null) return;
    }
    if (!mounted) return;
    await _act(
      () => _client.decide(
        widget.sessionId,
        a.approvalId,
        approved: approved,
        comment: comment?.isEmpty ?? true ? null : comment,
      ),
      done: approved ? 'Approved' : 'Rejected',
    );
  }

  Future<void> _stop() async {
    final reason = await promptText(
      context,
      title: 'Stop this agent?',
      action: 'STOP',
      hint: 'Reason (recorded in the audit log)',
      destructive: true,
    );
    if (reason == null || !mounted) return;
    await _act(() => _client.stop(widget.sessionId, reason.isEmpty ? null : reason), done: 'Stopped');
  }

  Future<void> _finish() async {
    final ok = await confirm(
      context,
      title: 'Finish session?',
      message: 'The session ends as completed. The agent will not continue.',
      action: 'Finish',
    );
    if (ok && mounted) await _act(() => _client.finish(widget.sessionId), done: 'Finished');
  }

  Future<void> _send() async {
    final text = _message.text.trim();
    if (text.isEmpty) return;
    if (await runAction(context, () => _client.sendMessage(widget.sessionId, text))) {
      _message.clear();
      await _load();
    }
  }

  @override
  Widget build(BuildContext context) {
    final detail = _detail;
    if (detail == null) {
      return Scaffold(
        appBar: AppBar(),
        body: Center(child: _error != null ? Text('$_error') : const CircularProgressIndicator()),
      );
    }
    final s = detail.session;
    final canOperate = AppScope.of(context).canOperate;
    final live = !s.status.isTerminal;
    return DefaultTabController(
      length: 2,
      child: Scaffold(
        appBar: AppBar(
          title: Text(s.agent),
          actions: [
            if (canOperate && live && s.status == SessionStatus.paused)
              IconButton(
                tooltip: 'Resume',
                icon: const Icon(Icons.play_arrow),
                onPressed: () => _act(() => _client.resume(s.id), done: 'Resumed'),
              ),
            if (canOperate && live && s.status != SessionStatus.paused)
              IconButton(
                tooltip: 'Pause',
                icon: const Icon(Icons.pause),
                onPressed: () => _act(() => _client.pause(s.id), done: 'Paused'),
              ),
            if (canOperate && live)
              IconButton(
                tooltip: 'Stop agent',
                icon: Icon(Icons.stop_circle, color: Theme.of(context).colorScheme.error),
                onPressed: _stop,
              ),
          ],
          bottom: const TabBar(
            tabs: [
              Tab(text: 'Activity'),
              Tab(text: 'Changes'),
            ],
          ),
        ),
        body: TabBarView(
          children: [
            Column(
              children: [
                _Header(detail: detail),
                for (final a in detail.approvals)
                  _ApprovalCard(approval: a, canDecide: canOperate, onDecide: (ok) => _decide(a, ok)),
                _filters(),
                const Divider(height: 1),
                Expanded(child: _timeline()),
                if (canOperate && s.status == SessionStatus.awaitingInput) _composer(),
              ],
            ),
            _ChangesTab(client: _client, sessionId: s.id),
          ],
        ),
      ),
    );
  }

  Widget _filters() => Padding(
    padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
    child: Wrap(
      spacing: 8,
      children: [
        FilterChip(
          label: const Text('Output'),
          selected: _showOutput,
          onSelected: (v) => setState(() => _showOutput = v),
        ),
        FilterChip(
          label: const Text('Policy details'),
          selected: _showDetails,
          onSelected: (v) => setState(() => _showDetails = v),
        ),
      ],
    ),
  );

  Widget _timeline() {
    final shown = _events.where((e) => (_showOutput || e.kind != 'output') && (_showDetails || !e.isDetail)).toList();
    if (shown.isEmpty) return const EmptyList('Waiting for events…', icon: Icons.hourglass_empty);
    final theme = Theme.of(context);
    return ListView.builder(
      controller: _scroll,
      padding: const EdgeInsets.symmetric(vertical: 8),
      itemCount: shown.length,
      itemBuilder: (context, i) {
        final e = shown[i];
        final output = e.kind == 'output';
        final stderr = output && e.raw['stream'] == 'stderr';
        return Padding(
          padding: EdgeInsets.symmetric(horizontal: 12, vertical: output ? 0 : 3),
          child: Row(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              SizedBox(
                width: 64,
                child: Text(
                  clock(e.timestamp),
                  style: theme.textTheme.bodySmall?.copyWith(
                    color: theme.colorScheme.outline,
                    fontFeatures: const [FontFeature.tabularFigures()],
                  ),
                ),
              ),
              Expanded(
                child: SelectableText(
                  e.summary,
                  style: output
                      ? TextStyle(fontFamily: 'monospace', fontSize: 12, color: stderr ? theme.colorScheme.error : null)
                      : TextStyle(color: _eventColor(e, theme), fontWeight: _important(e) ? FontWeight.w600 : null),
                ),
              ),
            ],
          ),
        );
      },
    );
  }

  bool _important(AgentEvent e) =>
      const {'approval_requested', 'user_message', 'session_ended', 'stop_requested', 'delivered'}.contains(e.kind);

  Color? _eventColor(AgentEvent e, ThemeData theme) => switch (e.kind) {
    'approval_requested' => Colors.orange,
    'stop_requested' => theme.colorScheme.error,
    'action_completed' when (e.raw['outcome'] as Json?)?['status'] != 'succeeded' => theme.colorScheme.error,
    'user_message' => theme.colorScheme.primary,
    _ => null,
  };

  Widget _composer() => SafeArea(
    top: false,
    child: Container(
      padding: const EdgeInsets.fromLTRB(12, 8, 8, 8),
      decoration: BoxDecoration(
        border: Border(top: BorderSide(color: Theme.of(context).dividerColor)),
      ),
      child: Row(
        children: [
          Expanded(
            child: TextField(
              controller: _message,
              minLines: 1,
              maxLines: 5,
              decoration: const InputDecoration(hintText: 'Reply to the agent…', border: InputBorder.none),
            ),
          ),
          IconButton(tooltip: 'Finish session', icon: const Icon(Icons.done_all), onPressed: _finish),
          IconButton.filled(tooltip: 'Send', icon: const Icon(Icons.send), onPressed: _send),
        ],
      ),
    ),
  );
}

class _Header extends StatelessWidget {
  const _Header({required this.detail});
  final SessionDetail detail;

  @override
  Widget build(BuildContext context) {
    final s = detail.session;
    final theme = Theme.of(context);
    final muted = TextStyle(color: theme.colorScheme.onSurfaceVariant);
    return ExpansionTile(
      tilePadding: const EdgeInsets.symmetric(horizontal: 12),
      title: Text(s.task, maxLines: 2, overflow: TextOverflow.ellipsis),
      subtitle: Padding(
        padding: const EdgeInsets.only(top: 4),
        child: Wrap(
          spacing: 8,
          children: [
            StatusChip.session(s.status, context: context),
            Text('${s.actions} actions · ${s.modelCalls} model calls', style: muted),
          ],
        ),
      ),
      expandedCrossAxisAlignment: CrossAxisAlignment.start,
      childrenPadding: const EdgeInsets.fromLTRB(12, 0, 12, 12),
      children: [
        SelectableText(s.task),
        const SizedBox(height: 8),
        _row('Policy', detail.policyName),
        if (detail.roleTitle != null) _row('Role', detail.roleTitle!),
        if (s.origin != null) _row('From', s.origin!),
        _row('Started by', s.createdBy),
        _row('Started', '${timeAgo(s.createdAt)}${s.endedAt != null ? ' · ended ${timeAgo(s.endedAt)}' : ''}'),
        if (s.pullRequestUrl != null) _row('Pull request', s.pullRequestUrl!),
        if (detail.proposal != null) _row('Proposed PR', '${detail.proposal!['title']}'),
        _row('Session', s.id),
      ],
    );
  }

  Widget _row(String label, String value) => Padding(
    padding: const EdgeInsets.only(top: 2),
    child: Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        SizedBox(
          width: 96,
          child: Text(label, style: const TextStyle(fontWeight: FontWeight.w600)),
        ),
        Expanded(child: SelectableText(value)),
      ],
    ),
  );
}

class _ApprovalCard extends StatelessWidget {
  const _ApprovalCard({required this.approval, required this.canDecide, required this.onDecide});

  final PendingApproval approval;
  final bool canDecide;
  final void Function(bool approved) onDecide;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Card(
      margin: const EdgeInsets.fromLTRB(12, 4, 12, 4),
      color: Color.alphaBlend(Colors.orange.withValues(alpha: 0.12), theme.colorScheme.surface),
      surfaceTintColor: Colors.transparent,
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                const Icon(Icons.gpp_maybe_outlined, color: Colors.orange),
                const SizedBox(width: 8),
                Expanded(child: Text('Needs your approval', style: theme.textTheme.titleSmall)),
                if (approval.expiresAt != null)
                  Text('expires ${clock(approval.expiresAt)}', style: theme.textTheme.bodySmall),
              ],
            ),
            const SizedBox(height: 8),
            SelectableText(approval.summary, style: const TextStyle(fontFamily: 'monospace')),
            if (approval.reason.isNotEmpty) ...[
              const SizedBox(height: 4),
              Text(approval.reason, style: theme.textTheme.bodySmall),
            ],
            if (canDecide) ...[
              const SizedBox(height: 8),
              Row(
                mainAxisAlignment: MainAxisAlignment.end,
                children: [
                  OutlinedButton(onPressed: () => onDecide(false), child: const Text('Reject')),
                  const SizedBox(width: 8),
                  FilledButton(onPressed: () => onDecide(true), child: const Text('Approve')),
                ],
              ),
            ],
          ],
        ),
      ),
    );
  }
}

class _ChangesTab extends StatelessWidget {
  const _ChangesTab({required this.client, required this.sessionId});
  final ApiClient client;
  final String sessionId;

  @override
  Widget build(BuildContext context) {
    return AsyncList<Changes?>(
      load: () => client.changes(sessionId),
      builder: (context, changes) {
        if (changes == null) {
          return const EmptyList('This session does not work on a repository.', icon: Icons.difference_outlined);
        }
        final theme = Theme.of(context);
        return ListView(
          padding: const EdgeInsets.all(12),
          children: [
            Text(
              '${changes.commits.length} commit(s) · ${changes.files.length} file(s) changed',
              style: theme.textTheme.titleSmall,
            ),
            const SizedBox(height: 8),
            for (final c in changes.commits)
              ListTile(
                dense: true,
                contentPadding: EdgeInsets.zero,
                leading: const Icon(Icons.commit),
                title: Text('${c['subject']}'),
                subtitle: Text('${'${c['sha']}'.padRight(7).substring(0, 7)} · ${c['author']}'),
              ),
            for (final f in changes.files)
              ListTile(
                dense: true,
                contentPadding: EdgeInsets.zero,
                leading: const Icon(Icons.description_outlined),
                title: Text('${f['path']}', style: const TextStyle(fontFamily: 'monospace')),
                trailing: Text('+${f['additions'] ?? '?'} −${f['deletions'] ?? '?'}'),
              ),
            if (changes.patch.isNotEmpty) ...[
              const Divider(),
              SingleChildScrollView(
                scrollDirection: Axis.horizontal,
                child: SelectableText.rich(
                  TextSpan(
                    children: [
                      for (final line in changes.patch.split('\n'))
                        TextSpan(
                          text: '$line\n',
                          style: TextStyle(
                            color: line.startsWith('+') && !line.startsWith('+++')
                                ? Colors.green.shade600
                                : line.startsWith('-') && !line.startsWith('---')
                                ? theme.colorScheme.error
                                : line.startsWith('@@')
                                ? theme.colorScheme.primary
                                : null,
                          ),
                        ),
                    ],
                  ),
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
                ),
              ),
              if (changes.truncated) const Text('Diff truncated. See the full diff in the web UI.'),
            ],
          ],
        );
      },
    );
  }
}
