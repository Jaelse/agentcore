import 'dart:async';

import 'package:flutter/material.dart';

import '../api/models.dart';
import '../state/app_state.dart';
import 'common.dart';
import 'new_session_screen.dart';
import 'session_screen.dart';

/// Every session on the server, newest first, refreshed every few seconds.
class SessionsScreen extends StatefulWidget {
  const SessionsScreen({super.key});

  @override
  State<SessionsScreen> createState() => _SessionsScreenState();
}

class _SessionsScreenState extends State<SessionsScreen> {
  final _list = GlobalKey<AsyncListState<List<SessionInfo>>>();
  Timer? _timer;
  bool _activeOnly = true;

  @override
  void initState() {
    super.initState();
    _timer = Timer.periodic(const Duration(seconds: 5), (_) => _list.currentState?.reload());
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  Future<void> _open(String id) async {
    await Navigator.push(context, MaterialPageRoute<void>(builder: (_) => SessionScreen(sessionId: id)));
    _list.currentState?.reload();
  }

  Future<void> _new() async {
    final created = await Navigator.push<SessionInfo>(
      context,
      MaterialPageRoute(builder: (_) => const NewSessionScreen(), fullscreenDialog: true),
    );
    if (created != null && mounted) await _open(created.id);
  }

  @override
  Widget build(BuildContext context) {
    final app = AppScope.of(context);
    return Scaffold(
      floatingActionButton: app.canOperate
          ? FloatingActionButton.extended(
              onPressed: _new,
              icon: const Icon(Icons.add),
              label: const Text('New session'),
            )
          : null,
      body: Column(
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 8, 12, 0),
            child: Row(
              children: [
                SegmentedButton<bool>(
                  segments: const [
                    ButtonSegment(value: true, label: Text('Active')),
                    ButtonSegment(value: false, label: Text('All')),
                  ],
                  selected: {_activeOnly},
                  onSelectionChanged: (s) => setState(() => _activeOnly = s.first),
                ),
              ],
            ),
          ),
          Expanded(
            child: AsyncList<List<SessionInfo>>(
              key: _list,
              load: () => app.client!.sessions(),
              builder: (context, sessions) {
                final shown = _activeOnly ? sessions.where((s) => !s.status.isTerminal).toList() : sessions;
                if (shown.isEmpty) {
                  return EmptyList(
                    _activeOnly ? 'No agent is working right now.' : 'No sessions yet.',
                    icon: Icons.terminal,
                  );
                }
                return ListView.separated(
                  padding: const EdgeInsets.only(bottom: 88),
                  itemCount: shown.length,
                  separatorBuilder: (_, _) => const Divider(height: 1),
                  itemBuilder: (context, i) => SessionTile(session: shown[i], onTap: () => _open(shown[i].id)),
                );
              },
            ),
          ),
        ],
      ),
    );
  }
}

class SessionTile extends StatelessWidget {
  const SessionTile({super.key, required this.session, required this.onTap});

  final SessionInfo session;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final s = session;
    final muted = Theme.of(context).colorScheme.onSurfaceVariant;
    return ListTile(
      onTap: onTap,
      title: Text(s.task, maxLines: 2, overflow: TextOverflow.ellipsis),
      subtitle: Padding(
        padding: const EdgeInsets.only(top: 4),
        child: Wrap(
          spacing: 8,
          runSpacing: 4,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            StatusChip.session(s.status, context: context),
            if (s.pendingApprovals > 0) StatusChip('${s.pendingApprovals} to approve', color: Colors.orange),
            Text(s.agent, style: TextStyle(color: muted)),
            if (s.origin != null) Text(s.origin!, style: TextStyle(color: muted)),
            Text(timeAgo(s.createdAt), style: TextStyle(color: muted)),
          ],
        ),
      ),
      trailing: const Icon(Icons.chevron_right),
    );
  }
}
