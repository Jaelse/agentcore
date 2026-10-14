import 'dart:async';

import 'package:flutter/material.dart';

import '../api/client.dart';
import '../api/models.dart';
import '../state/app_state.dart';
import 'common.dart';
import 'session_screen.dart';

class _Inbox {
  _Inbox(this.approvals, this.outbox, this.releases, this.products);
  final List<(SessionInfo, PendingApproval)> approvals;
  final List<OutboxItem> outbox;
  final List<Release> releases;
  final Map<String, String> products;

  bool get isEmpty => approvals.isEmpty && outbox.isEmpty && releases.isEmpty;
}

/// Everything waiting for a person: agent actions, outgoing messages and
/// releases.
class InboxScreen extends StatefulWidget {
  const InboxScreen({super.key});

  @override
  State<InboxScreen> createState() => _InboxScreenState();
}

class _InboxScreenState extends State<InboxScreen> {
  final _list = GlobalKey<AsyncListState<_Inbox>>();
  Timer? _timer;

  @override
  void initState() {
    super.initState();
    _timer = Timer.periodic(const Duration(seconds: 10), (_) => _list.currentState?.reload());
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  Future<_Inbox> _load(ApiClient client) async {
    final sessions = await client.sessions();
    final waiting = sessions.where((s) => s.pendingApprovals > 0 && !s.status.isTerminal).toList();
    final approvals = <(SessionInfo, PendingApproval)>[];
    final perSession = await Future.wait(waiting.map((s) => client.approvals(s.id)));
    for (var i = 0; i < waiting.length; i++) {
      approvals.addAll(perSession[i].map((a) => (waiting[i], a)));
    }
    final outbox = client.outbox(status: 'pending');
    final releases = client.releases(status: 'proposed');
    final products = client.productNames().catchError((_) => <String, String>{});
    return _Inbox(approvals, await outbox, await releases, await products);
  }

  Future<void> _then(Future<bool> action) async {
    if (await action) _list.currentState?.reload();
  }

  @override
  Widget build(BuildContext context) {
    final app = AppScope.of(context);
    final client = app.client!;
    final canOperate = app.canOperate;
    return AsyncList<_Inbox>(
      key: _list,
      load: () => _load(client),
      builder: (context, inbox) {
        if (inbox.isEmpty) return const EmptyList('Nothing is waiting for you.', icon: Icons.task_alt);
        return ListView(
          padding: const EdgeInsets.only(bottom: 24),
          children: [
            if (inbox.approvals.isNotEmpty) _section(context, 'Agent actions', inbox.approvals.length),
            for (final (session, approval) in inbox.approvals)
              _InboxCard(
                icon: Icons.gpp_maybe_outlined,
                color: Colors.orange,
                title: approval.summary,
                monospace: true,
                subtitle: '${session.agent} · ${session.task}',
                body: approval.reason,
                onOpen: () => Navigator.push(
                  context,
                  MaterialPageRoute<void>(builder: (_) => SessionScreen(sessionId: session.id)),
                ).then((_) => _list.currentState?.reload()),
                actions: canOperate
                    ? [
                        OutlinedButton(
                          onPressed: () async {
                            final comment = await promptText(
                              context,
                              title: 'Reject this action?',
                              action: 'Reject',
                              hint: 'Why (the agent sees this)',
                              destructive: true,
                            );
                            if (comment == null || !context.mounted) return;
                            await _then(
                              runAction(
                                context,
                                () => client.decide(
                                  session.id,
                                  approval.approvalId,
                                  approved: false,
                                  comment: comment.isEmpty ? null : comment,
                                ),
                                done: 'Rejected',
                              ),
                            );
                          },
                          child: const Text('Reject'),
                        ),
                        FilledButton(
                          onPressed: () => _then(
                            runAction(
                              context,
                              () => client.decide(session.id, approval.approvalId, approved: true),
                              done: 'Approved',
                            ),
                          ),
                          child: const Text('Approve'),
                        ),
                      ]
                    : const [],
              ),
            if (inbox.outbox.isNotEmpty) _section(context, 'Outgoing messages', inbox.outbox.length),
            for (final item in inbox.outbox)
              _InboxCard(
                icon: Icons.outgoing_mail,
                color: Colors.blue,
                title: item.subject.isEmpty ? '(no subject)' : item.subject,
                subtitle: 'To ${item.recipients.join(', ')} · drafted by ${item.draftedBy}',
                body: item.body,
                actions: canOperate
                    ? [
                        TextButton(
                          onPressed: () async {
                            final text = await promptText(
                              context,
                              title: 'Reject message?',
                              action: 'Reject',
                              hint: 'Why',
                              required: true,
                              destructive: true,
                            );
                            if (text == null || !context.mounted) return;
                            await _then(runAction(context, () => client.rejectOutbox(item.id, text), done: 'Rejected'));
                          },
                          child: const Text('Reject'),
                        ),
                        OutlinedButton(
                          onPressed: () async {
                            final text = await promptText(
                              context,
                              title: 'Send back for changes',
                              action: 'Send back',
                              hint: 'What should change?',
                              required: true,
                            );
                            if (text == null || !context.mounted) return;
                            await _then(
                              runAction(
                                context,
                                () => client.outboxChanges(item.id, text),
                                done: 'Sent back to the agent',
                              ),
                            );
                          },
                          child: const Text('Changes'),
                        ),
                        FilledButton(
                          onPressed: () async {
                            final ok = await confirm(
                              context,
                              title: 'Send this message?',
                              message: 'It goes out to ${item.recipients.join(', ')} now.',
                              action: 'Send',
                            );
                            if (!ok || !context.mounted) return;
                            await _then(
                              runAction(context, () => client.sendOutbox(item.id, item.revision), done: 'Sending'),
                            );
                          },
                          child: const Text('Send'),
                        ),
                      ]
                    : const [],
              ),
            if (inbox.releases.isNotEmpty) _section(context, 'Releases', inbox.releases.length),
            for (final r in inbox.releases)
              _InboxCard(
                icon: Icons.rocket_launch_outlined,
                color: switch (r.riskLevel) {
                  'high' => Colors.red,
                  'medium' => Colors.orange,
                  _ => Colors.green,
                },
                title: '${inbox.products[r.productId] ?? 'Release'} ${r.version}',
                subtitle:
                    '${r.riskLevel} risk · ${r.approvedBy.length}/${r.approvalsRequired} approvals · proposed by ${r.proposedBy}',
                body: [r.riskAssessment, r.notes].where((t) => t.isNotEmpty).join('\n\n'),
                actions: canOperate
                    ? [
                        TextButton(
                          onPressed: () async {
                            final text = await promptText(
                              context,
                              title: 'Reject release?',
                              action: 'Reject',
                              hint: 'Why',
                              required: true,
                              destructive: true,
                            );
                            if (text == null || !context.mounted) return;
                            await _then(runAction(context, () => client.rejectRelease(r.id, text), done: 'Rejected'));
                          },
                          child: const Text('Reject'),
                        ),
                        OutlinedButton(
                          onPressed: () async {
                            final text = await promptText(
                              context,
                              title: 'Request changes',
                              action: 'Send back',
                              hint: 'What should change?',
                              required: true,
                            );
                            if (text == null || !context.mounted) return;
                            await _then(runAction(context, () => client.releaseChanges(r.id, text), done: 'Sent back'));
                          },
                          child: const Text('Changes'),
                        ),
                        FilledButton(
                          onPressed: () async {
                            final ok = await confirm(
                              context,
                              title: 'Approve ${r.version}?',
                              message:
                                  'The server verifies every artifact before counting your approval. '
                                  'Approve on the web UI if you want to review artifacts, SBOM and checks first.',
                              action: 'Approve',
                            );
                            if (!ok || !context.mounted) return;
                            await _then(
                              runAction(context, () => client.approveRelease(r.id, r.revision), done: 'Approved'),
                            );
                          },
                          child: const Text('Approve'),
                        ),
                      ]
                    : const [],
              ),
          ],
        );
      },
    );
  }

  Widget _section(BuildContext context, String title, int count) => Padding(
    padding: const EdgeInsets.fromLTRB(16, 16, 16, 4),
    child: Text('$title ($count)', style: Theme.of(context).textTheme.titleSmall),
  );
}

class _InboxCard extends StatefulWidget {
  const _InboxCard({
    required this.icon,
    required this.color,
    required this.title,
    required this.subtitle,
    required this.body,
    required this.actions,
    this.monospace = false,
    this.onOpen,
  });

  final IconData icon;
  final Color color;
  final String title;
  final String subtitle;
  final String body;
  final List<Widget> actions;
  final bool monospace;
  final VoidCallback? onOpen;

  @override
  State<_InboxCard> createState() => _InboxCardState();
}

class _InboxCardState extends State<_InboxCard> {
  bool _expanded = false;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    return Card(
      margin: const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
      clipBehavior: Clip.antiAlias,
      child: InkWell(
        onTap: widget.onOpen ?? () => setState(() => _expanded = !_expanded),
        child: Padding(
          padding: const EdgeInsets.all(12),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Icon(widget.icon, color: widget.color),
                  const SizedBox(width: 10),
                  Expanded(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          widget.title,
                          style: theme.textTheme.titleSmall?.copyWith(
                            fontFamily: widget.monospace ? 'monospace' : null,
                          ),
                        ),
                        const SizedBox(height: 2),
                        Text(
                          widget.subtitle,
                          maxLines: 2,
                          overflow: TextOverflow.ellipsis,
                          style: theme.textTheme.bodySmall,
                        ),
                      ],
                    ),
                  ),
                  if (widget.onOpen != null) const Icon(Icons.chevron_right),
                ],
              ),
              if (widget.body.isNotEmpty) ...[
                const SizedBox(height: 8),
                Text(widget.body, maxLines: _expanded ? null : 4, overflow: _expanded ? null : TextOverflow.fade),
              ],
              if (widget.actions.isNotEmpty) ...[
                const SizedBox(height: 8),
                SizedBox(
                  width: double.infinity,
                  child: Wrap(alignment: WrapAlignment.end, spacing: 8, runSpacing: 4, children: widget.actions),
                ),
              ],
            ],
          ),
        ),
      ),
    );
  }
}
